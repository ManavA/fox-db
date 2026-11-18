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
            "1 call sites in 1 files\nmay be incomplete: `wake_task` also appears in 2 other files; confirm with: rg -nw wake_task"
        ),
        "{out}"
    );
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
        assert!(out.contains(&format!("{lead}: `helper` also appears in {n} other files")), "{rel}: {out}");
    }
    let out = fox_code(&p, "simple_open", "refs", 800);
    assert!(out.contains("may be incomplete: `simple_open` also appears in 1 other file;"), "{out}");
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
    let out = fox_code(&p, "Partial.ToConfig", "impact", 2000);
    assert!(!out.contains("Run"), "{out}");
    let out = fox_code(&p, "Getter.ToConfig", "impact", 2000);
    assert!(out.contains("Flags.ToConfig") && !out.contains("Partial.ToConfig"), "{out}");
    // A type named in a class body (a field's type) is no supertype.
    let out = fox_code(&p, "Service.save", "impact", 2000);
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
    // `pick_one` has two definitions: the calls in y/sub/ and z/ are guessed to the other one
    // (nearer, or first of a tie), so they may still reach `base` through this one.
    let out = run("base", 2);
    assert!(out.contains(":1 pick_one c:") && !out.contains("user c:") && !out.contains("other c:"), "{out}");
    assert!(
        out.contains("\npartial at d2: calls in 2 other files were guessed to another `pick_one`; they may reach a d1 dependent"),
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
