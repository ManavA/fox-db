//! Differential test of incremental indexing: a tree is indexed once, then edited step by step;
//! after each edit the incremental index must hold exactly what a fresh full index of the same
//! tree holds (every reference's target and ambiguity, every symbol's kind), so `callers`,
//! `refs` and `impact` answer the same.
use foxdb::{db, inventory, project, tsindex};
use std::path::Path;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

/// One edit: write (path, Some(text)) or remove (path, None) files.
type Edit<'a> = (&'a str, &'a [(&'a str, Option<&'a str>)]);

fn set_home(h: &Path) {
    // SAFETY: tests touching FOXDB_HOME hold SERIAL.
    unsafe { std::env::set_var("FOXDB_HOME", h) };
}

fn index(repo: &Path) -> project::Project {
    let mut c = db::open_rw().unwrap();
    let p = project::ensure(&c, &project::locate(repo)).unwrap();
    inventory::index_project(&mut c, &p).unwrap();
    tsindex::index_project(&mut c, &p).unwrap();
    p
}

/// What the index holds, without ids: references with their targets, and symbol kinds.
fn dump() -> Vec<String> {
    let c = db::open_ro().unwrap();
    let mut out: Vec<String> = vec![];
    let mut q = c
        .prepare(
            "SELECT r.path, r.line, r.name, r.kind, r.amb, COALESCE(n.path,''), COALESCE(n.name,''), COALESCE(n.line,0),
                    CASE WHEN r.slot < 0 THEN '-' ELSE '' END, COALESCE(s.path,''), COALESCE(s.name,''), COALESCE(s.line,0)
             FROM ts_ref r LEFT JOIN node n ON n.id = r.dst AND r.dst != 0 LEFT JOIN node s ON s.id = abs(r.slot) AND r.slot != 0",
        )
        .unwrap();
    let rows = q
        .query_map([], |r| {
            Ok(format!(
                "ref {}:{} {} {} amb={} -> {}:{}:{} slot={}{}:{}:{}",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, i64>(11)?,
            ))
        })
        .unwrap();
    out.extend(rows.flatten());
    let mut q = c
        .prepare("SELECT path, name, line, COALESCE(json_extract(meta,'$.k'),'') FROM node WHERE kind = 'symbol' AND source = 'treesitter'")
        .unwrap();
    let rows = q
        .query_map([], |r| {
            Ok(format!("sym {}:{} {} {}", r.get::<_, String>(0)?, r.get::<_, i64>(2)?, r.get::<_, String>(1)?, r.get::<_, String>(3)?))
        })
        .unwrap();
    out.extend(rows.flatten());
    out.sort();
    out
}

/// Index `files`, apply `edits` one by one, and after each compare the incremental index with a
/// fresh full index of the same tree.
fn check(files: &[(&str, &str)], edits: &[Edit]) {
    let inc_home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let f = repo.path().join(path);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }
    set_home(inc_home.path());
    index(repo.path());
    for (what, changes) in edits {
        for (path, text) in *changes {
            let f = repo.path().join(path);
            match text {
                Some(t) => {
                    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                    std::fs::write(&f, t).unwrap();
                }
                None => std::fs::remove_file(&f).unwrap(),
            }
        }
        set_home(inc_home.path());
        index(repo.path());
        let inc = dump();
        let full_home = tempfile::tempdir().unwrap();
        set_home(full_home.path());
        index(repo.path());
        let full = dump();
        set_home(inc_home.path());
        let only_inc: Vec<&String> = inc.iter().filter(|x| !full.contains(x)).collect();
        let only_full: Vec<&String> = full.iter().filter(|x| !inc.contains(x)).collect();
        assert!(only_inc.is_empty() && only_full.is_empty(), "after {what}:\nincremental only: {only_inc:#?}\nfull only: {only_full:#?}");
    }
}

const PY_BASE: &str = "class Base:\n    def run(self):\n        return self.hook()\n\n    def hook(self):\n        return 0\n";
const PY_MIXIN: &str = "class Mixin:\n    def hook(self):\n        return 1\n\n    def step(self):\n        return 2\n";

#[test]
fn python_edits_match_a_full_index() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    check(
        &[
            ("base.py", PY_BASE),
            ("mixin.py", PY_MIXIN),
            ("pa/__init__.py", ""),
            ("pa/base.py", PY_BASE),
            ("pb/__init__.py", ""),
            ("pb/base.py", "class Base:\n    def other(self):\n        return 9\n"),
            (
                "sub.py",
                "from base import Base\nfrom mixin import Mixin\n\n\nclass Sub(Base, Mixin):\n    def go(self):\n        return self.step()\n",
            ),
            ("deep.py", "from sub import Sub\n\n\nclass Deep(Sub):\n    def dd(self):\n        return self.hook()\n"),
            ("imp.py", "from pa.base import Base\n\n\nclass Imp(Base):\n    def ii(self):\n        return self.hook()\n"),
            ("imp2.py", "from pa.base import Base\nfrom mixin import Mixin\n\n\nclass Imp2(Mixin, Base):\n    pass\n"),
            ("sup.py", "from base import Base\n\n\nclass S2(Base):\n    def hook(self):\n        return super().hook()\n"),
        ],
        &[
            (
                "reorder bases",
                &[(
                    "sub.py",
                    Some(
                        "from base import Base\nfrom mixin import Mixin\n\n\nclass Sub(Mixin, Base):\n    def go(self):\n        return self.step()\n",
                    ),
                )],
            ),
            (
                "re-target an import",
                &[("imp.py", Some("from pb.base import Base\n\n\nclass Imp(Base):\n    def ii(self):\n        return self.hook()\n"))],
            ),
            (
                "re-target a subclass's import",
                &[("imp2.py", Some("from pb.base import Base\nfrom mixin import Mixin\n\n\nclass Imp2(Mixin, Base):\n    pass\n"))],
            ),
            (
                "add a subclass file",
                &[("sub2.py", Some("from base import Base\nfrom mixin import Mixin\n\n\nclass Sub2(Mixin, Base):\n    pass\n"))],
            ),
            (
                "add a lonely class",
                &[(
                    "lonely.py",
                    Some("class Lonely:\n    def zz(self):\n        return self.yy()\n\n    def yy(self):\n        return 1\n"),
                )],
            ),
            (
                "add a method",
                &[(
                    "deep.py",
                    Some(
                        "from sub import Sub\n\n\nclass Deep(Sub):\n    def dd(self):\n        return self.hook()\n\n    def hook(self):\n        return 5\n",
                    ),
                )],
            ),
            (
                "shadow with an attribute",
                &[(
                    "mixin.py",
                    Some("def _x(self):\n    return 0\n\n\nclass Mixin:\n    hook = _x\n\n    def step(self):\n        return 2\n"),
                )],
            ),
            (
                "expression base",
                &[(
                    "sub2.py",
                    Some("from base import Base\n\n\ndef make():\n    return object\n\n\nclass Sub2(make(), Base):\n    pass\n"),
                )],
            ),
            (
                "add a method to a base",
                &[(
                    "base.py",
                    Some(
                        "class Base:\n    def run(self):\n        return self.hook()\n\n    def hook(self):\n        return 0\n\n    def step(self):\n        return 3\n",
                    ),
                )],
            ),
            (
                "change a root's bases",
                &[("base.py", Some("from mixin import Mixin\n\n\nclass Base(Mixin):\n    def run(self):\n        return self.hook()\n"))],
            ),
            ("add a third same-name base", &[("pc/base.py", Some("class Base:\n    def hook(self):\n        return 7\n"))]),
            ("remove it again", &[("pc/base.py", None)]),
            (
                "make an import name the only base",
                &[(
                    "sub.py",
                    Some(
                        "from pa.base import Base\nfrom mixin import Mixin\n\n\nclass Sub(Mixin, Base):\n    def go(self):\n        return self.step()\n",
                    ),
                )],
            ),
            ("remove a subclass file", &[("sub2.py", None)]),
            ("remove a base file", &[("mixin.py", None)]),
            ("restore the base file", &[("mixin.py", Some(PY_MIXIN))]),
        ],
    );
}

#[test]
fn cpp_edits_match_a_full_index() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    check(
        &[
            ("base.hpp", "class Engine { public: void start() {} };\nclass Pump { public: void start() {} };\n"),
            ("a.hpp", "#include \"base.hpp\"\nnamespace other { struct Car { Engine engine; Pump* pump; }; }\n"),
            (
                "u.cpp",
                "#include \"a.hpp\"\n#include <memory>\nvoid drive(other::Car& c) { c.engine.start(); c.pump->start(); }\nvoid local() { Engine e; e.start(); }\nvoid up(std::unique_ptr<Pump> p) { p->start(); }\nint ext(std::string& s) { return s.size(); }\nvoid al(a::E2& e) { e.start(); }\n",
            ),
            ("al.hpp", "#include \"base.hpp\"\nnamespace a { using E2 = Engine; }\n"),
            ("t.hpp", "typedef int (*op_fn)(int);\nstruct Ops { op_fn run; };\n"),
            (
                "t.cpp",
                "#include \"t.hpp\"\nstatic int impl(int x) { return x; }\nint use(Ops *o) { return o->run(1); }\nOps g = { impl };\n",
            ),
        ],
        &[
            (
                "add a same-name class in a namespace",
                &[("b.hpp", Some("#include \"base.hpp\"\nnamespace other { class Engine : public Pump {}; }\n"))],
            ),
            ("retype a field", &[("a.hpp", Some("#include \"base.hpp\"\nnamespace other { struct Car { Pump engine; Pump* pump; }; }\n"))]),
            ("remove the class", &[("b.hpp", None)]),
            (
                "add an overload",
                &[(
                    "base.hpp",
                    Some("class Engine { public: void start() {} void start(int) {} };\nclass Pump { public: void start() {} };\n"),
                )],
            ),
            (
                "make a method virtual",
                &[("base.hpp", Some("class Engine { public: virtual void start() {} };\nclass Pump { public: void start() {} };\n"))],
            ),
            ("retype the typedef", &[("t.hpp", Some("typedef int op_fn;\nstruct Ops { op_fn run; };\n"))]),
            ("restore the typedef", &[("t.hpp", Some("typedef int (*op_fn)(int);\nstruct Ops { op_fn run; };\n"))]),
            ("add a library-named class", &[("s.hpp", Some("namespace mylib { class string { public: int size() { return 0; } }; }\n"))]),
            ("add a class named like an alias", &[("e2.hpp", Some("namespace q { class E2 { public: void start() {} }; }\n"))]),
            ("add a class", &[("z.cpp", Some("class Zed { public: void start() {} };\n"))]),
        ],
    );
}

#[test]
fn c_edits_match_a_full_index() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    check(
        &[
            ("types.h", "typedef int (*op_fn)(int);\ntypedef op_fn op2_fn;\n"),
            ("ops.h", "#include \"types.h\"\nstruct ops {\n\top_fn run;\n\top2_fn stop;\n\tint (*plain)(int);\n};\n"),
            ("impl.c", "#include \"ops.h\"\nstatic int a_run(int x) { return x; }\nstatic struct ops a_ops = {\n\t.run = a_run,\n};\n"),
            ("call.c", "#include \"ops.h\"\nint drive(struct ops *o) { return o->run(1) + o->stop(2) + o->plain(3); }\n"),
        ],
        &[
            ("retype the typedef", &[("types.h", Some("typedef int op_fn;\ntypedef op_fn op2_fn;\n"))]),
            ("restore it", &[("types.h", Some("typedef int (*op_fn)(int);\ntypedef op_fn op2_fn;\n"))]),
            (
                "store another function",
                &[(
                    "impl2.c",
                    Some("#include \"ops.h\"\nstatic int b_run(int x) { return x; }\nvoid b_init(struct ops *o) { o->run = b_run; }\n"),
                )],
            ),
            (
                "retype a field",
                &[("ops.h", Some("#include \"types.h\"\nstruct ops {\n\tint run;\n\top2_fn stop;\n\tint (*plain)(int);\n};\n"))],
            ),
            ("remove a file", &[("impl.c", None)]),
        ],
    );
}

/// A name defined only in a subclass becomes reachable from an ancestor's call when a class in
/// between gains or loses that ancestor as a base (also for a call in a nested function).
#[test]
fn python_descendant_names_follow_base_changes() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let a = "class A:\n    def run(self):\n        return self.x()\n";
    let a_nested = "class A:\n    def run(self):\n        def inner():\n            return self.x()\n        return inner()\n";
    let c_plain = "class C:\n    pass\n";
    let c_sub = "from a import A\n\n\nclass C(A):\n    pass\n";
    let d = "from c import C\n\n\nclass D(C):\n    def x(self):\n        return 1\n";
    // n1, then n2 (its reverse).
    check(
        &[("a.py", a), ("c.py", c_plain), ("d.py", d)],
        &[("C gains A", &[("c.py", Some(c_sub))]), ("C loses A", &[("c.py", Some(c_plain))])],
    );
    // n14.
    check(&[("a.py", a_nested), ("c.py", c_plain), ("d.py", d)], &[("C gains A", &[("c.py", Some(c_sub))])]);
}
