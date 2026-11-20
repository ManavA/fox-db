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
        "8 lines name `fetch_page`: 1 call (1 guessed), 1 definition, 1 import, 2 in 1 same-name definition elsewhere, 2 comments, 1 in strings (may be dynamic uses); may be incomplete: 1 line reaches the target only through a guess by name (marked guess); confirm it",
        "{out}"
    );
    assert!(out.contains("callers of ~c:") && out.contains(":8 run c:"), "{out}");
    // Only the guess keeps it from reading complete: comments, strings and the other class's
    // method are counted, not listed.
    assert!(!out.contains("lib/other.py") && !out.contains("app/run.py:6") && !out.contains("unresolved mentions"), "{out}");
    // `refs` and `impact` account for the same lines.
    let out = fox_code(&p, "Fetcher.fetch_page", "refs", 2000);
    assert!(
        first(&out).starts_with("8 lines name `fetch_page`: 1 ref (1 guessed), 1 definition,")
            && first(&out).contains("; may be incomplete: 1 line reaches the target only through a guess by name"),
        "{out}"
    );
    let out = fox_code(&p, "Fetcher.fetch_page", "impact", 2000);
    assert!(
        first(&out).starts_with("8 lines name `fetch_page`: 1 use (d1), 1 definition,")
            && first(&out).contains("; partial at d1: 1 line reaches the target only through a guess by name"),
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
        "7 lines name `plan_query`: 2 calls, 1 definition, 2 declarations/prototypes, 1 comment, 1 unresolved; may be incomplete: 1 unresolved line may use it",
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
    assert_eq!(
        first(&out),
        "4 lines name `plan_query`: 1 call, 1 definition, 2 declarations/prototypes; all accounted for in indexed C/C++/Objective-C/Swift code",
        "{out}"
    );
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

/// A common name stays short at every budget: counts for every group and a few rows.
#[test]
fn common_names_list_a_few_rows_at_any_budget() {
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
    for budget in [400, 4000] {
        let out = fox_code(&p, "close_all", "callers", budget);
        assert!(first(&out).starts_with("63 lines name `close_all`: 1 call, 1 definition, 1 import, 30 comments, 30 unresolved;"), "{out}");
        assert!(out.lines().filter(|l| l.starts_with("  mod/")).count() <= 20, "{out}");
        assert!(out.contains("more lines are counted, not listed") && !out.contains("comment lines"), "{out}");
    }
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
        "4 lines name `NewFake`: 1 call, 1 definition, 1 in 1 same-name definition elsewhere, 1 comment; all accounted for in indexed Go code",
        "{out}"
    );
    let out = fox_code(&p, "stamp", "callers", 2000);
    assert_eq!(first(&out), "3 lines name `stamp`: 1 call, 1 definition, 1 import; all accounted for in indexed JS/TS code", "{out}");
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
        "5 lines name `must_defer`: 1 call, 1 definition, 1 import, 2 declarations/prototypes; all accounted for in indexed Python code",
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
    assert!(!first(&out).contains("all accounted for"), "{out}");
}

// ---- review fixes: no line the ledger cannot classify reads as accounted for ----

fn ends_complete(out: &str) -> bool {
    first(out).contains("; all accounted for in indexed ")
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

/// Freshness: a file whose time moved but whose content did not is current; the upgrade's
/// re-parse marker reads as pending; a file added since indexing (in a new directory too) or an
/// uncited file edited in place to name the target makes it stale, named once; a file too big
/// to index that names it makes it incomplete.
#[test]
fn freshness_is_proven() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, r, p) = inline_project(PY);
    let f = r.path().join("app/run.py");
    let t = std::fs::File::options().write(true).open(&f).unwrap();
    t.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
    drop(t);
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(!first(&out).contains("stale") && first(&out).contains("guess by name"), "touched only: {out}");
    let bump = || {
        let c = db::open_rw().unwrap();
        db::bump_generation(&c).unwrap();
    };
    {
        let c = db::open_rw().unwrap();
        c.execute("UPDATE ts_file SET size = -1 WHERE path = 'lib/other.py'", []).unwrap();
        db::bump_generation(&c).unwrap();
    }
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(first(&out).contains("may be incomplete: the index is being updated"), "migration marker: {out}");
    let (_h, r, p) = inline_project(PY);
    std::fs::create_dir_all(r.path().join("new/deep")).unwrap();
    std::fs::write(r.path().join("new/deep/more.py"), "def more(f):\n    return f.fetch_page(1)\n").unwrap();
    bump();
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(first(&out).contains("the index is stale for 1 file (new/deep/more.py)"), "added: {out}");
    assert_eq!(first(&out).matches("new/deep/more.py").count(), 1, "{out}");
    let (_h, r, p) = inline_project(&[PY[0], PY[1], PY[2], ("app/quiet.py", "def quiet():\n    return 1\n")]);
    std::fs::write(r.path().join("app/quiet.py"), "def quiet(f):\n    return f.fetch_page(2)\n").unwrap();
    bump();
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(first(&out).contains("the index is stale for 1 file (app/quiet.py)"), "edited: {out}");
    let (_h, r, p) = inline_project(PY);
    let big = format!("def big(f):\n    return f.fetch_page(3)\n{}", "x = 1\n".repeat(400_000));
    std::fs::write(r.path().join("app/big.py"), big).unwrap();
    {
        let mut c = db::open_rw().unwrap();
        inventory::index_project(&mut c, &p).unwrap();
        tsindex::index_project(&mut c, &p).unwrap();
    }
    let out = fox_code(&p, "Fetcher.fetch_page", "callers", 2000);
    assert!(first(&out).contains("may be incomplete: 1 file too big to index name it (app/big.py)"), "big: {out}");
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
        assert!(listed <= 20, "{budget}: {listed}");
        assert!(!out.contains("(first 0 of") && !out.contains("lists them all"), "{budget}: {out}");
        let first_row = out.lines().skip_while(|l| !l.starts_with("unresolved mentions")).nth(1).unwrap_or("");
        assert!(first_row.contains("zz/use.py:2"), "{budget}: {out}");
    }
}

/// Go: a call through a local or parameter of another type never resolves with certainty to a
/// same-file method; the method's own receiver does.
#[test]
fn go_locals_of_another_type_are_not_certain() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module ex\ngo 1.21\n"),
        (
            "a.go",
            "package ex\n\ntype Server struct{}\n\nfunc (s *Server) plan() int { return 1 }\n\nfunc (s *Server) run() int {\n\treturn s.plan()\n}\n\nfunc h2() int {\n\tx := &Other{}\n\treturn x.plan()\n}\n\nfunc h3(y *Other) int {\n\treturn y.plan()\n}\n",
        ),
        ("b.go", "package ex\n\ntype Other struct{}\n\nfunc (o *Other) plan() int { return 2 }\n"),
    ]);
    let out = fox_code(&p, "Server.plan", "callers", 2000);
    let row = |n: &str| out.lines().find(|l| l.contains(n)).unwrap_or("").to_string();
    assert!(row("Server.run c:").starts_with("  :8 ") && !row("Server.run c:").contains("guess"), "{out}");
    for other in ["h2 c:", "h3 c:"] {
        assert!(row(other).is_empty() || row(other).contains("guess"), "{other}: {out}");
    }
    assert!(!ends_complete(&out), "{out}");
}

/// Swift calls reach Objective-C methods: one family for accounting and resolution.
#[test]
fn swift_reaches_objective_c() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("Core.m", "#import <Foundation/Foundation.h>\n@implementation Core\n- (int)plan { return 1; }\n@end\n"),
        ("Use.swift", "func go(o: Core) -> Int {\n    return o.plan()\n}\n"),
    ]);
    let out = fox_code(&p, "plan", "callers", 2000);
    assert!(out.contains("Use.swift"), "{out}");
}

/// A bare call of a builtin with a method's name (`len(x)` for `T.len`) is no candidate.
#[test]
fn bare_builtin_calls_are_not_candidates_for_methods() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, _r, p) = inline_project(&[
        ("go.mod", "module ex\ngo 1.21\n"),
        (
            "q.go",
            "package ex\n\ntype Q struct{ xs []int }\n\nfunc (q *Q) len() int { return len(q.xs) }\n\nfunc use(q *Q) int { return q.len() + len(q.xs) }\n",
        ),
    ]);
    let out = fox_code(&p, "Q.len", "callers", 2000);
    assert!(ends_complete(&out), "{out}");
}

/// Strings and comments come from the parse: nested JS templates, JSX text, Dart and Ruby
/// interpolation, PHP heredocs and Python raw f-strings never hide a use.
#[test]
fn parsed_strings_never_hide_a_use() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "js",
            &[
                ("plan.js", "export class A { plan(x) { return x; } }\n"),
                (
                    "use.js",
                    "export function go(ok, b, o) {\n  const a = `x ${ok ? `y ${b}` : ''} z`;\n  const f1 = o.plan;\n  return `w`;\n}\n",
                ),
            ],
        ),
        (
            "jsx",
            &[
                ("a.js", "export class A { plan(x) { return x; } }\n"),
                ("App.jsx", "export function C({ done, o }) {\n  return <span>{done} / <Btn onClick={o.plan} /></span>;\n}\n"),
            ],
        ),
        ("dart", &[("a.dart", "class A { int plan(int x) => x; }\n"), ("use.dart", "void go(dynamic o) {\n  print('v ${o.plan}');\n}\n")]),
        (
            "rb",
            &[
                ("a.rb", "class A\n  def plan(x)\n    x\n  end\nend\n"),
                ("use.rb", "def go(o)\n  s = \"line one\n#{o.method(:plan)} two\"\n  s\nend\n"),
            ],
        ),
        (
            "php",
            &[
                ("a.php", "<?php\nclass A { public function plan($x) { return $x; } }\n"),
                (
                    "use.php",
                    "<?php\nfunction go($o) {\n    $doc = <<<EOT\nFiles under src/*.php\nEOT;\n    $f = $o->plan;\n    return $doc;\n}\n",
                ),
            ],
        ),
        (
            "py",
            &[
                ("a.py", "class A:\n    def plan(self, x):\n        return x\n"),
                ("use.py", "def go(o):\n    a = fr\"{o.plan}\"\n    b = Fr\"\"\"{o.plan}\"\"\"\n    return a, b\n"),
            ],
        ),
    ];
    for (what, files) in cases {
        let (_h, _r, p) = inline_project(files);
        let out = fox_code(&p, "A.plan", "refs", 2000);
        assert!(!ends_complete(&out) && !first(&out).contains("comment") && !first(&out).contains("strings"), "{what}: {out}");
    }
}

/// (what, target, rel, files)
type Case<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, &'a str)]);

/// Every occurrence on a line needs its own reference or definition: a second, unresolved use
/// beside a resolved one, an alias (`plan = core.plan`) or a delegating wrapper, an import and a
/// call on one line all leave the line unresolved.
#[test]
fn every_occurrence_on_a_line_is_covered() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let cases: &[Case] = &[
        (
            "alias",
            "core.plan",
            "callers",
            &[
                ("core.py", "def plan(x):\n    return x\n"),
                ("use.py", "import core\nplan = core.plan\n\n\ndef go():\n    return plan(1)\n"),
            ],
        ),
        (
            "second use",
            "A.plan",
            "callers",
            &[(
                "a.py",
                "class A:\n    def plan(self):\n        return 1\n\n    def run(self, b):\n        return self.plan(), register(b.plan)\n\n\nclass B:\n    def plan(self):\n        return 2\n",
            )],
        ),
        (
            "import and call",
            "A.plan",
            "refs",
            &[
                ("a.py", "class A:\n    def plan(self):\n        return 1\n\n\nclass B:\n    def plan(self):\n        return 2\n"),
                ("other.py", "def plan():\n    return 3\n"),
                ("use.py", "from other import plan; o.plan()\n"),
            ],
        ),
        (
            "wrapper",
            "A.plan",
            "callers",
            &[
                ("a.js", "export class A { plan(x) { return x; } }\n"),
                ("b.js", "export class B { plan(x) { return this.inner.plan(x); } }\n"),
            ],
        ),
        (
            "go method value",
            "Server.plan",
            "refs",
            &[
                ("go.mod", "module ex\ngo 1.21\n"),
                (
                    "a.go",
                    "package ex\n\ntype Server struct{}\n\nfunc (s *Server) plan() int { return 1 }\n\ntype Other struct{}\n\nfunc (o *Other) plan() int { return 2 }\n\nfunc (o *Other) run(y *Server) func() int {\n\to.plan(); return y.plan\n}\n",
                ),
            ],
        ),
        (
            "python class body",
            "A.plan",
            "callers",
            &[(
                "a.py",
                "class A:\n    def plan(x):\n        return x\n\n    y = plan(3)\n\n\nclass B:\n    def plan(self):\n        return 1\n\n\ndef go(b):\n    return b.plan()\n",
            )],
        ),
    ];
    for (what, q, rel, files) in cases {
        let (_h, _r, p) = inline_project(files);
        let out = fox_code(&p, q, rel, 2000);
        assert!(!ends_complete(&out), "{what}: {out}");
    }
}

/// Swift `\(x)` and Kotlin `$x` in strings are code.
#[test]
fn swift_and_kotlin_templates_are_code() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for files in [
        &[
            ("A.swift", "let plan = 3\nfunc go() -> String {\n    return \"v \\(plan)\"\n}\n"),
            ("B.swift", "func plan() -> Int { return 1 }\n"),
        ][..],
        &[
            ("A.kt", "package ex\nval plan = 3\nfun go(): String {\n    return \"v $plan\"\n}\n"),
            ("B.kt", "package ex\nfun plan(): Int = 1\n"),
        ][..],
    ] {
        let (_h, _r, p) = inline_project(files);
        let target = if files[0].0.ends_with(".swift") { "B.plan" } else { "ex.plan" };
        let out = fox_code(&p, target, "refs", 2000);
        assert!(!first(&out).contains("in strings") && !ends_complete(&out), "{out}");
    }
}

/// A warm server sees edits: an edit to a file the answer read, a new file, or a changed
/// ignore rule since the index makes the next answer (a second later) stale.
#[test]
fn a_warm_answer_sees_edits_and_ignore_changes() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_h, r, p) = inline_project(&[
        ("core.py", "def plan(x):\n    return x\n\n\ndef go():\n    return plan(1)\n"),
        ("use.py", "from core import plan\n\n\ndef other():\n    return plan(5)\n"),
    ]);
    let out = fox_code(&p, "core.plan", "callers", 2000);
    assert!(ends_complete(&out), "{out}");
    std::fs::write(
        r.path().join("use.py"),
        "from core import plan\n\n\ndef other():\n    return plan(5)\n\n\ndef more():\n    return plan(6)\n",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let out = fox_code(&p, "core.plan", "callers", 2000);
    assert!(first(&out).contains("stale") && first(&out).contains("use.py"), "edited: {out}");
    let (_h, r, p) = inline_project(&[
        (".gitignore", "gen/\n"),
        ("core.py", "def plan(x):\n    return x\n\n\ndef go():\n    return plan(1)\n"),
        ("gen/g.py", "from core import plan\n\n\ndef g():\n    return plan(2)\n"),
    ]);
    let out = fox_code(&p, "core.plan", "callers", 2000);
    assert!(ends_complete(&out), "{out}");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(r.path().join(".gitignore"), "").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let out = fox_code(&p, "core.plan", "callers", 2000);
    assert!(first(&out).contains("stale") && first(&out).contains("gen/g.py"), "ignore changed: {out}");
}
