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
        "8 lines name `fetch_page`: 1 call (1 guessed), 1 definition, 1 import, 2 in 1 same-name definition elsewhere, 3 comments/strings; all accounted for",
        "{out}"
    );
    assert!(out.contains("callers of ~c:") && out.contains(":8 run c:"), "{out}");
    assert!(out.contains("import lines:\n  app/run.py:2 | from lib import fetch_page"), "{out}");
    assert!(out.contains("same-name definitions elsewhere (different symbols; 1 line inside or using them not listed):\n  lib/other.py:2 method Mirror.fetch_page c:"), "{out}");
    assert!(out.contains("comments and strings:\n  app/run.py:6 | # fetch_page is slow"), "{out}");
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
        "7 lines name `plan_query`: 2 calls, 1 definition, 2 declarations/prototypes, 1 comment/string, 1 unresolved; may be incomplete: 1 unresolved line may use it, listed under unresolved mentions",
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
    assert!(
        first(&out).ends_with("; the index is stale for 1 cited file (app/run.py): counts may be off; reindex (fox index) first"),
        "{out}"
    );
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
    assert!(
        first(&out).starts_with("63 lines name `close_all`: 1 call, 1 definition, 1 import, 30 comments/strings, 30 unresolved;"),
        "{out}"
    );
    assert!(out.len() <= 400 * 4 + 200, "{} bytes\n{out}", out.len());
    assert!(out.contains("unresolved mentions (first ") && out.contains("comments and strings (first 2 of 30):"), "{out}");
    let need: usize = out
        .lines()
        .find_map(|l| l.strip_prefix("rows left out to fit the budget; budget=").and_then(|r| r.split(' ').next()?.parse().ok()))
        .unwrap_or_else(|| panic!("no budget named\n{out}"));
    let out = fox_code(&p, "close_all", "callers", need);
    assert!(out.contains("unresolved mentions:\n") && !out.contains("rows left out"), "{out}");
    assert_eq!(out.lines().filter(|l| l.starts_with("  mod/") && l.contains("[x.close_all")).count(), 30, "{out}");
    // Comments are counted and only a few listed, whatever the budget.
    assert!(out.contains("comments and strings (first 5 of 30):"), "{out}");
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
        "4 lines name `NewFake`: 1 call, 1 definition, 1 in 1 same-name definition elsewhere, 1 comment/string; all accounted for",
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
