//! Line accounting on `callers`, `refs` and `impact`: the answer is a labelled superset of
//! `rg -w name` over the indexed code. Its first line counts every line naming the target by
//! group; the groups the answer does not list as resolved rows follow at the end.

use foxdb::{code, db, inventory, project, tsindex};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

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

fn fox_code(p: &project::Project, sym: &str, rel: &str, budget: usize) -> String {
    let c = db::open_ro().unwrap();
    code::run(&c, p, sym, &code::CodeOpts { rel, depth: 2, budget, cursor: 0, fallback: false }).unwrap()
}

fn first(out: &str) -> &str {
    out.lines().next().unwrap_or("")
}

const PY: &[(&str, &str)] = &[
    (
        "lib/fetch.py",
        "class Fetcher:\n    def fetch_page(self, url):\n        \"\"\"Fetch one page; see fetch_page in the docs.\"\"\"\n        return url\n",
    ),
    ("lib/other.py", "class Mirror:\n    def fetch_page(self, url):\n        return self.fetch_page(url)\n"),
    (
        "app/run.py",
        "from lib.fetch import Fetcher\nfrom lib import fetch_page\n\n\ndef run(fetcher: Fetcher):\n    # fetch_page is slow\n    log = \"calling fetch_page\"\n    return fetcher.fetch_page(\"u\")\n",
    ),
];

/// Every line naming a Python method is accounted for: its call, its definition, an import,
/// a same-name method of another class (and the line inside it), a docstring, a comment and a
/// string.
#[test]
fn python_lines_are_accounted_for_by_group() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(PY);
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert_eq!(
        first(&out),
        "8 lines name `fetch_page`: 1 call (1 guessed), 1 definition, 1 import, 2 in 1 same-name definition elsewhere, 2 comments, 1 in strings (may be dynamic uses); all accounted for",
        "{out}"
    );
    assert!(out.contains("callers of ~c:") && out.contains(":8 run c:"), "{out}");
    assert!(out.contains("import lines:\n  app/run.py:2 | from lib import fetch_page"), "{out}");
    assert!(out.contains("same-name definitions elsewhere (different symbols; 1 line using them not listed):\n  lib/other.py:2 method Mirror.fetch_page c:"), "{out}");
    assert!(out.contains("comment lines:\n  app/run.py:6 | # fetch_page is slow"), "{out}");
    assert!(out.contains("lib/fetch.py:3 |"), "{out}");
    // The line inside the other definition is counted, not listed.
    assert!(!out.contains("lib/other.py:3"), "{out}");
    assert!(!out.contains("unresolved mentions") && !out.contains("may be incomplete"), "{out}");
    // `refs` and `impact` account for the same lines.
    let out = fox_code(&p, "Fetcher.fetch_page", "refs", 2000);
    assert!(
        first(&out).starts_with("8 lines name `fetch_page`: 1 ref (1 guessed), 1 definition,")
            && first(&out).ends_with("all accounted for"),
        "{out}"
    );
    let out = fox_code(&p, "Fetcher.fetch_page", "impact", 2000);
    assert!(
        first(&out).starts_with("8 lines name `fetch_page`: 1 use (d1), 1 definition,") && first(&out).ends_with("all accounted for"),
        "{out}"
    );
    assert!(out.contains("impact of ~c:"), "{out}");
}

/// C header prototypes and `extern` declarations are their own group; a line that may use the
/// function (a function pointer initializer) is unresolved, listed, and makes the answer
/// read as incomplete.
#[test]
fn c_prototypes_are_declarations_and_unknown_uses_are_listed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("include/plan.h", "#ifndef PLAN_H\nint plan_query(int q);\nextern int plan_query(int q);\n#endif\n"),
        ("src/plan.c", "#include \"plan.h\"\nint plan_query(int q) { return q; }\nint run(void) { return plan_query(1); }\n"),
        (
            "src/other.c",
            "/* plan_query is defined in plan.c */\nint use(void) { return plan_query(2); }\nstatic int (*fp)(int) = plan_query;\n",
        ),
    ]);
    let out = fox_code(&p, "plan_query", "callers", 2000);
    assert_eq!(
        first(&out),
        "7 lines name `plan_query`: 2 calls, 1 definition, 2 declarations/prototypes, 1 comment, 1 unresolved; may be incomplete: 1 unresolved line may use it, listed under unresolved mentions",
        "{out}"
    );
    assert!(out.contains("unresolved mentions:\n  src/other.c:3 | static int (*fp)(int) = plan_query;"), "{out}");
    assert!(out.contains("declarations and prototypes:\n  include/plan.h:2 function plan_query c:"), "{out}");
    assert!(out.contains("include/plan.h:3 "), "{out}");
    // Without that line, every line is accounted for.
    let (_h, _r, p) = inline_project(&[
        ("include/plan.h", "int plan_query(int q);\nstruct plan_query;\n"),
        ("src/plan.c", "#include \"plan.h\"\nint plan_query(int q) { return q; }\nint run(void) { return plan_query(1); }\n"),
    ]);
    let out = fox_code(&p, "plan_query", "callers", 2000);
    assert_eq!(first(&out), "4 lines name `plan_query`: 1 call, 1 definition, 2 declarations/prototypes; all accounted for", "{out}");
}

/// A cited file changed since it was indexed: the accounting says so instead of claiming
/// every line is accounted for.
#[test]
fn a_stale_index_is_named() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, r, p) = inline_project(PY);
    let f = r.path().join("app/run.py");
    let mut text = std::fs::read_to_string(&f).unwrap();
    text.push_str("\n# fetch_page again\n");
    std::fs::write(&f, text).unwrap();
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(first(&out).ends_with("; the index is stale for 1 file (app/run.py): counts may be off; reindex (fox index) first"), "{out}");
    assert!(!out.contains("all accounted for"), "{out}");
}

/// A common name stays within a small budget: counts for every group, a few rows each, and
/// the budget that lists them all; that budget does.
#[test]
fn common_names_keep_to_the_budget_and_name_the_budget_that_lists_all() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> = vec![
        ("lib/sock.py".into(), "def close_all(s):\n    return s\n".into()),
        ("app/use.py".into(), "from lib.sock import close_all\n\nclose_all(1)\n".into()),
    ];
    for i in 0..30 {
        files.push((
            format!("mod/m{i:02}.py"),
            format!("# close_all is called elsewhere\nhandlers_{i} = [x.close_all for x in items_{i}]\n"),
        ));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, _r, p) = inline_project(&refs);
    let out = fox_code(&p, "close_all", "callers", 400);
    assert!(first(&out).starts_with("63 lines name `close_all`: 1 call, 1 definition, 1 import, 30 comments, 30 unresolved;"), "{out}");
    assert!(out.len() <= 400 * 4 + 200, "{} bytes\n{out}", out.len());
    assert!(out.contains("unresolved mentions (first ") && out.contains("comment lines (first 2 of 30):"), "{out}");
    let need: usize = out
        .lines()
        .find_map(|l| l.strip_prefix("rows left out to fit the budget; budget=").and_then(|r| r.split(' ').next()?.parse().ok()))
        .unwrap_or_else(|| panic!("no budget named\n{out}"));
    let out = fox_code(&p, "close_all", "callers", need);
    assert!(out.contains("unresolved mentions:\n") && !out.contains("rows left out"), "{out}");
    assert_eq!(out.lines().filter(|l| l.starts_with("  mod/") && l.contains("[x.close_all")).count(), 30, "{out}");
    // Comments are counted and only a few listed, whatever the budget.
    assert!(out.contains("comment lines (first 5 of 30):"), "{out}");
}

/// A name with no indexed definition that grep would find is never an empty answer.
#[test]
fn a_name_without_a_definition_lists_its_lines() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[("app/out.py", "from fastjson import dumps_safe\n\n\ndef emit(x):\n    return dumps_safe(x)\n")]);
    let out = fox_code(&p, "dumps_safe", "callers", 2000);
    assert!(
        first(&out).starts_with("2 lines name `dumps_safe`: 1 import, 1 unresolved; no definition of `dumps_safe` is indexed"),
        "{out}"
    );
    assert!(out.contains("app/out.py:5 | return dumps_safe(x)") && out.contains("app/out.py:1 |"), "{out}");
    // A name nothing mentions still says no symbol matches.
    let out = fox_code(&p, "nowhere_at_all", "callers", 2000);
    assert!(out.contains("no symbol matches"), "{out}");
}

/// Go: a same-name method of another type is a different symbol; a TypeScript import is an
/// import.
#[test]
fn go_and_typescript_groups() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("clock/fake.go", "package clock\n\ntype Fake struct{}\n\n// NewFake makes a Fake.\nfunc NewFake() *Fake { return &Fake{} }\n"),
        ("other/fake.go", "package other\n\nfunc NewFake() int { return 1 }\n"),
        ("srv/srv.go", "package srv\n\nimport \"example.com/clock\"\n\nfunc Run() { _ = clock.NewFake() }\n"),
        ("web/a.ts", "export function stamp(x: number) { return x }\n"),
        ("web/b.ts", "import { stamp } from './a'\n\nexport const y = stamp(1)\n"),
    ]);
    let out = fox_code(&p, "clock.NewFake", "callers", 2000);
    assert_eq!(
        first(&out),
        "4 lines name `NewFake`: 1 call, 1 definition, 1 in 1 same-name definition elsewhere, 1 comment; all accounted for",
        "{out}"
    );
    let out = fox_code(&p, "stamp", "callers", 2000);
    assert_eq!(first(&out), "3 lines name `stamp`: 1 call, 1 definition, 1 import; all accounted for", "{out}");
}

/// Python `@overload` stubs and a property's setter are no symbols of their own: the name
/// resolves to the one definition, calls to it are certain, and the stub lines are declarations.
#[test]
fn python_overload_stubs_and_setters_merge_into_one_symbol() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        (
            "lib/defer.py",
            "from typing import overload\n\n@overload\ndef must_defer(x: int) -> int: ...\n@overload\ndef must_defer(x: str) -> str: ...\ndef must_defer(x):\n    return x\n\nclass Box:\n    @property\n    def size(self):\n        return 1\n    @size.setter\n    def size(self, v):\n        pass\n",
        ),
        ("app/use.py", "from lib.defer import must_defer\n\n\ndef run():\n    return must_defer(1)\n"),
    ]);
    let out = fox_code(&p, "must_defer", "callers", 2000);
    assert!(!out.contains("matches") && !out.contains("guess"), "{out}");
    assert!(out.contains("callers of ~c:") && out.contains("lib/defer.py:7"), "{out}");
    assert_eq!(
        first(&out),
        "5 lines name `must_defer`: 1 call, 1 definition, 1 import, 2 declarations/prototypes; all accounted for",
        "{out}"
    );
    let out = fox_code(&p, "Box.size", "callers", 2000);
    assert!(!out.contains("matches"), "{out}");
}

/// A use inside another same-name definition (a delegating wrapper) is listed when it may
/// reach the target, not hidden as part of that definition.
#[test]
fn a_wrapper_inside_a_same_name_definition_is_listed() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("lib/engine.py", "class Engine:\n    def plan_query(self, q):\n        return q\n"),
        ("lib/proxy.py", "class Proxy:\n    def plan_query(self, q):\n        return self.inner.plan_query(q)\n"),
    ]);
    let out = fox_code(&p, "Engine.plan_query", "callers", 2000);
    assert!(out.contains("lib/proxy.py:3 "), "{out}");
    assert!(!first(&out).ends_with("all accounted for"), "{out}");
}

// ---- review fixes: no line the ledger cannot classify reads as accounted for ----

fn ends_complete(out: &str) -> bool {
    first(out).ends_with("all accounted for")
}

/// A line naming the target inside another same-name definition with no reference row (a Rust
/// macro argument) is unresolved, not part of that definition.
#[test]
fn lines_without_a_reference_inside_same_name_definitions_are_unresolved() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("Cargo.toml", "[package]\nname=\"rs4\"\nversion=\"0.1.0\"\n"),
        (
            "src/main.rs",
            "mod inner;\n\nfn plan() -> i32 {\n    println!(\"{}\", inner::plan());\n    2\n}\n\nfn main() {\n    let _ = plan();\n}\n",
        ),
        ("src/inner.rs", "pub fn plan() -> i32 {\n    1\n}\n"),
    ]);
    let out = fox_code(&p, "inner.plan", "callers", 2000);
    assert!(!ends_complete(&out) && out.contains("unresolved mentions:\n  src/main.rs:4 "), "{out}");
}

/// Multi-line strings keep their state across lines: a `/*` inside one opens no comment, so
/// a later call is not hidden as a comment.
#[test]
fn multi_line_strings_do_not_open_comments() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("Cargo.toml", "[package]\nname=\"rs\"\nversion=\"0.1.0\"\n"),
        (
            "src/main.rs",
            "fn plan() -> i32 {\n    1\n}\n\nfn main() {\n    let usage = \"usage:\n  fox index src/*.rs\n\";\n    assert_eq!(plan(), 1);\n    let _ = usage;\n}\n",
        ),
    ]);
    let out = fox_code(&p, "plan", "callers", 2000);
    assert!(!out.contains("comment") && out.contains("src/main.rs:9 "), "{out}");
    let (_h, _r, p) = inline_project(&[
        (
            "Outer.kt",
            "class Outer {\n    fun go(): Int = 1\n}\n\nval help = \"\"\"\n  globs: src/*.kt\n\"\"\"\n\nfun main() {\n    println(listOf(1).map { plan() })\n}\n",
        ),
        ("Plan.kt", "fun plan(): Int = 1\n"),
    ]);
    let out = fox_code(&p, "plan", "callers", 2000);
    assert!(!first(&out).contains("comment"), "{out}");
}

/// Uncertain masking never makes a line a comment or a string: a backtick in a JS regex, an
/// apostrophe in JSX text, a Rust raw string ending in a backslash.
#[test]
fn uncertain_masking_is_not_a_comment() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("a.js", "export function plan() {\n  return 1;\n}\n"),
        (
            "re.js",
            "import { plan } from './a.js';\n\nconst QUOTE = /[`'\"]/;\nexport function q(s) {\n  return QUOTE.test(s);\n}\nexport const handler = plan;\n",
        ),
        ("App.jsx", "export function App() {\n  return <div><span>It's here</span>{plan()}</div>;\n}\n"),
    ]);
    let out = fox_code(&p, "plan", "refs", 2000);
    assert!(!first(&out).contains("comment") && !first(&out).contains("in strings"), "{out}");
    let (_h, _r, p) = inline_project(&[
        ("Cargo.toml", "[package]\nname=\"rs\"\nversion=\"0.1.0\"\n"),
        ("src/main.rs", "fn plan() -> i32 {\n    1\n}\n\nfn main() {\n    let p = r\"C:\\\"; let _ = plan();\n    let _ = p;\n}\n"),
    ]);
    let out = fox_code(&p, "plan", "callers", 2000);
    assert!(!first(&out).contains("in strings") && !first(&out).contains("comment"), "{out}");
}

/// A call through a module or another type's value never resolves with certainty to a
/// same-name definition of its own file.
#[test]
fn qualified_calls_do_not_resolve_to_the_same_file_definition() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("core.py", "def plan():\n    return 1\n"),
        (
            "wrap.py",
            "import core\n\n\nclass A:\n    def plan(self):\n        return core.plan()\n\n\ndef plan():\n    return core.plan()\n",
        ),
    ]);
    let out = fox_code(&p, "core.plan", "callers", 2000);
    assert!(out.contains(" wrap.py (2)") || out.contains("wrap.py:6") && out.contains("wrap.py:10"), "{out}");
    let (_h, _r, p) = inline_project(&[
        (
            "Outer.kt",
            "class Outer(val inner: Inner) {\n    fun plan(): Int {\n        return inner.plan()\n    }\n}\n\nfun main() {\n    val i = Inner()\n    println(listOf(1).map { i.plan() })\n}\n",
        ),
        ("Inner.kt", "class Inner {\n    fun plan(): Int = 1\n}\n"),
    ]);
    let out = fox_code(&p, "Outer.plan", "callers", 2000);
    assert!(out.contains("0 call sites"), "{out}");
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module ex.com/gor\ngo 1.21\n"),
        ("server.go", "package main\n\ntype Server struct{ inner *Inner }\n\nfunc (s *Server) Plan() int {\n\treturn s.inner.Plan()\n}\n"),
        ("inner.go", "package main\n\ntype Inner struct{}\n\nfunc (i *Inner) Plan() int { return 1 }\n"),
    ]);
    let out = fox_code(&p, "Server.Plan", "callers", 2000);
    assert!(out.contains("0 call sites"), "{out}");
}

/// A renamed import (`plan as p`, a default import under another name) may bind the target
/// under a name the answer does not follow: unresolved.
#[test]
fn renamed_imports_are_unresolved() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("a.js", "export function plan() {\n  return 1;\n}\n"),
        ("b.js", "import { plan as p } from './a.js';\n\nexport function run() {\n  return p();\n}\n"),
        ("core.py", "def plan():\n    return 1\n"),
        ("alias.py", "from core import plan as make\n\n\ndef go():\n    return make()\n"),
    ]);
    for (q, at) in [("a.plan", "b.js:1 "), ("core.plan", "alias.py:1 ")] {
        let out = fox_code(&p, q, "callers", 2000);
        assert!(!ends_complete(&out) && out.contains(at), "{q}: {out}");
    }
    let (_h, _r, p) = inline_project(&[
        ("plan.ts", "export default function plan(x: number): number {\n  return x;\n}\n"),
        ("use.ts", "import doIt from './plan';\n\nexport function go(): number {\n  return doIt(1);\n}\n"),
    ]);
    let out = fox_code(&p, "plan", "callers", 2000);
    assert!(!ends_complete(&out) && out.contains("use.ts:1 "), "{out}");
}

/// Staleness: a file whose time moved but whose content did not is current; the upgrade's
/// re-parse marker is no staleness; a file added since indexing that names the target is, and
/// no file is named twice.
#[test]
fn staleness_is_precise() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, r, p) = inline_project(PY);
    let f = r.path().join("app/run.py");
    let t = std::fs::File::options().write(true).open(&f).unwrap();
    t.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
    drop(t);
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(ends_complete(&out), "touched only: {out}");
    {
        let c = db::open_rw().unwrap();
        c.execute("UPDATE ts_file SET size = -1 WHERE path = 'lib/other.py'", []).unwrap();
    }
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(ends_complete(&out), "migration marker: {out}");
    std::fs::write(r.path().join("app/new.py"), "from lib.fetch import Fetcher\n\n\ndef more(f: Fetcher):\n    return f.fetch_page(1)\n")
        .unwrap();
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(first(&out).contains("the index is stale for 1 file (app/new.py)"), "added: {out}");
    assert_eq!(first(&out).matches("app/new.py").count(), 1, "{out}");
}

/// Only files of the target's language count, a Python stub line of another symbol is no
/// declaration of this one, and names in strings say they may be dynamic uses.
#[test]
fn language_stubs_and_strings() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("core.py", "def plan():\n    return 1\n"),
        ("web/plan.js", "export function other() { return 'plan' }\n"),
        (
            "m.py",
            "from typing import overload\n\n\nclass C:\n    @overload\n    def plan(self, x: int) -> int: ...\n    def plan(self, x):\n        return x\n",
        ),
        ("dyn.py", "import core\n\n\ndef go():\n    return getattr(core, \"plan\")()\n"),
    ]);
    let out = fox_code(&p, "core.plan", "callers", 2000);
    assert!(!out.contains("web/plan.js") && !first(&out).contains("declaration"), "{out}");
    assert!(first(&out).contains("1 in strings (may be dynamic uses)"), "{out}");
}

/// Big ledgers: rows are capped at every budget (the rest counted, no promise to list them
/// all), no group header goes without rows, and unresolved rows whose receiver is named like
/// the target's type come first.
#[test]
fn big_ledgers_cap_rows_and_lead_with_plausible_receivers() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut files: Vec<(String, String)> = vec![
        ("lib/store.py".into(), "class Store:\n    def fetch(self, k):\n        return k\n".into()),
        ("zz/use.py".into(), "def use(store):\n    return store.fetch(1)\n".into()),
    ];
    for i in 0..400 {
        files.push((format!("mod/m{i:03}.py"), format!("def f{i}(o):\n    return o.fetch({i})\n")));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (_h, _r, p) = inline_project(&refs);
    for budget in [400, 3999, 4000] {
        let out = fox_code(&p, "Store.fetch", "callers", budget);
        let listed = out.lines().filter(|l| l.starts_with("  mod/")).count();
        assert!(listed <= 200, "{budget}: {listed}");
        assert!(!out.contains("(first 0 of") && !out.contains("lists them all"), "{budget}: {out}");
        let first_row = out.lines().skip_while(|l| !l.starts_with("unresolved mentions")).nth(1).unwrap_or("");
        assert!(first_row.contains("zz/use.py:2"), "{budget}: {out}");
    }
}
