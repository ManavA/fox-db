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

/// An answer without its trailing `unresolved mentions:` rows.
fn resolved(out: &str) -> String {
    out.split("unresolved mentions:\n").next().unwrap_or(out).to_string()
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
            "1 call sites in 1 files
may be incomplete: `wake_task` is also named in 2 other files, not resolved to it; unresolved mentions at the end"
        ),
        "{out}"
    );
    // The mentions are listed with their code, not left to a grep.
    assert!(
        out.contains(
            "unresolved mentions:
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

/// A one-word name whose other uses have no reference row (a function-pointer initializer, a
/// field assignment, a macro body, an argument) still gets the completeness line, from every
/// identifier of every file, not only from indexed symbol text.
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
    // The argument in src/arg.c is a resolved reference: listed by `refs` and `impact`, still
    // unknown to `callers` (what `apply` calls through it is not followed).
    for (rel, lead, n) in [("callers", "may be incomplete", 4), ("refs", "may be incomplete", 3), ("impact", "partial at d1", 3)] {
        let out = fox_code(&p, "helper", rel, 800);
        assert!(out.contains(&format!("{lead}: `helper` is also named in {n} other files")), "{rel}: {out}");
    }
    let out = fox_code(&p, "simple_open", "refs", 800);
    assert!(out.contains("may be incomplete: `simple_open` is also named in 1 other file,"), "{out}");
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
    assert!(out.contains(" tools/main.py\n  :3 (top)\n"), "{out}");
    let out = fox_code(&p, "getColor", "impact", 2000);
    assert!(out.contains(" src/utils/color.test.ts\n  :5 (top)\n"), "{out}");
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
    assert!(out.contains(":3 add_all c:") && !out.contains("pass_on"), "{out}");
    let out = fox_code(&p, "worker", "impact", 2000);
    assert!(out.contains(":7 Run c:") && !out.contains("Other"), "{out}");
    let out = fox_code(&p, "spider_opened", "impact", 2000);
    assert!(out.contains(":3 LogCount.from_crawler c:"), "{out}");
    let out = fox_code(&p, "spider_closed", "impact", 2000);
    assert!(out.contains(":3 LogCount.from_crawler c:"), "{out}");
    let out = fox_code(&p, "onError", "impact", 2000);
    assert!(out.contains(":3 setup c:") && !out.contains("relay"), "{out}");
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
    let out = fox_code(&p, "Getter.ToConfig", "impact", 2000);
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
        ("x/a.c", "int pick_one(void) { return base(); }\n"),
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
    assert!(out.contains("Conn.Close") && !out.contains("Writer.Close"), "{out}");
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
    assert!(out.contains("Pipe.Close") && !out.contains("File.Close"), "{out}");
    let out = fox_code(&p, "IOCloser.Close", "impact", 2000);
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
    let out = fox_code(&p, "ExtHandler.handle", "impact", 2000);
    assert!(!out.contains("useLocal"), "{out}");
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
    let out = fox_code(&p, "Logger.Printf", "impact", 2000);
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
    // Impact rows carry the dependent's definition line.
    let out = fox_code(&p, "wake_task", "impact", 800);
    assert!(out.contains(":2 run c:") && out.contains(" | int run(void) {"), "{out}");
    // A budget the code does not fit in: locations only, and a note.
    let out = fox_code(&p, "wake_task", "callers", 66);
    assert!(out.len() <= 66 * 4 + 80, "{} bytes: {out}", out.len());
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
