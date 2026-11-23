"""Oracle tests on captured compiler output and small sources: python3 -m unittest bench/oracle/test_diag.py"""

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
DATA = HERE / "testdata"
sys.path.insert(0, str(HERE))
import diag  # noqa: E402
import foxcli  # noqa: E402
import oracle  # noqa: E402
import scan  # noqa: E402
from oracle import Skip, add_param, call_line  # noqa: E402


def c_macro_def(f, ln):
    return scan.c_in_define((DATA / f).read_text(), ln)


class Gcc(unittest.TestCase):
    """testdata/gcc16-mutated.txt: gcc 16 (LC_ALL=C) on testdata/t.c after `target` gained a parameter."""

    def setUp(self):
        self.ds = diag.parse_gcc((DATA / "gcc16-mutated.txt").read_text())

    def sites(self):
        out = []
        for d in self.ds:
            f, ln = d.site(c_macro_def)
            out.append((f, ln, d.func_at(f), diag.classify_c(d, "target")))
        return out

    def test_each_site_is_classified_and_named(self):
        self.assertEqual(self.sites(), [
            ("t.h", 3, "in_header", "call"),
            ("t.c", 5, None, "store"),
            ("t.c", 7, None, "store"),
            ("t.c", 9, "user1", "call"),
            ("t.c", 10, "user2", "store"),
            ("t.c", 10, "user2", "store"),
            ("t.c", 10, "user2", "call"),  # call written in CALL_T's definition: placed at its use
            ("t.c", 11, "user3", "store"),
            ("t.c", 12, "user4", "other"),
            ("t.c", 17, "user5", "call"),  # call written in CHECK's argument: its own line
            ("t.c", 25, "user7", "other"),  # too few arguments to another function
            ("t.c", 26, None, "decl"),
        ])

    def test_conflict_points_at_the_prototype(self):
        self.assertEqual(diag.conflicting_decls(self.ds, "target"), [("t.h", 1)])

    def test_unicode_quotes_and_one_line_rows(self):
        ds = diag.parse_gcc("a.c: In function ‘f’:\na.c:3:9: error: too few arguments to function ‘g’; expected 2, have 1\n")
        self.assertEqual([(d.func_at("a.c"), diag.classify_c(d, "g"), diag.classify_c(d, "h")) for d in ds],
                         [("f", "call", "other")])

    def test_store_must_name_the_target(self):
        d = diag.Diag("a.c", 1, 1, "error", "initialization of 'fn' from incompatible pointer type 'int (*)(int)'",
                      notes=[("a.c", 2, 1, "'other' declared here")])
        self.assertEqual(diag.classify_c(d, "target"), "other")


class Rustc(unittest.TestCase):
    """testdata/rustc-mutated.jsonl: cargo check of testdata/lib.rs after `target` gained a parameter."""

    def test_json_errors(self):
        src = (DATA / "lib.rs").read_text()
        defs = scan.rust_macro_defs(src)
        in_def = lambda f, ln: any(a <= ln <= b for a, b in defs)  # noqa: E731
        ds = diag.parse_rustc_json((DATA / "rustc-mutated.jsonl").read_text())
        got = sorted({(*d.site(in_def), d.code, diag.classify_rust(d, "target")) for d in ds})
        self.assertEqual(got, [
            ("src/lib.rs", 6, "E0061", "call"), ("src/lib.rs", 7, "E0308", "store"),
            ("src/lib.rs", 8, "E0593", "store"), ("src/lib.rs", 8, "E0599", "other"),
            ("src/lib.rs", 9, "E0061", "call"),
            ("src/lib.rs", 27, "E0061", "call"),  # argument of a std macro call spanning lines
            ("src/lib.rs", 31, "E0061", "call"),  # written in twice!'s body: placed at its use
            ("src/lib.rs", 31, "E0061", "other"),  # other(1): another function
        ])

    def test_store_when_name_anywhere_in_span(self):
        d = diag.Diag("a.rs", 1, 1, "error", "mismatched types", code="E0308", span_text="Self::target")
        self.assertEqual(diag.classify_rust(d, "target"), "store")
        d.span_text = "target_two"
        self.assertEqual(diag.classify_rust(d, "target"), "other")


class GoErrors(unittest.TestCase):
    def test_build_errors(self):
        ds = diag.parse_go((DATA / "go-mutated.txt").read_text())
        got = [(d.file, d.line, diag.classify_go(d, "target")) for d in ds]
        self.assertEqual(got, [("./t.go", 6, "call"), ("./t.go", 7, "store"), ("./t.go", 8, "store")])

    def test_call_must_name_the_target(self):
        d = diag.Diag("a.go", 1, 1, "error", "not enough arguments in call to t.other")
        self.assertEqual(diag.classify_go(d, "target"), "other")
        d.msg = "not enough arguments in call to r.target"
        self.assertEqual(diag.classify_go(d, "target"), "call")

    def test_call_line_is_the_call_start(self):
        src = b'func f() {\n\tx := target(1,\n\t\t"a)",\n\t)\n}\n'
        self.assertEqual(call_line(src, 4, 2, "target"), 2)

    @unittest.skipUnless(shutil.which("go"), "needs go")
    def test_goast(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "a.go").write_text("package a\n\ntype T[K any] struct{}\n\nfunc (t *T[K]) run() {\n}\n\nfunc Top() {}\n")
            subprocess.run(["go", "build", "-o", f"{d}/goast", "."], cwd=HERE / "goast", check=True)
            out = subprocess.run([f"{d}/goast", "a.go"], cwd=d, capture_output=True, text=True, check=True).stdout
        self.assertIn('"name":"run","recv":"T","line":5,"end":6,"exported":false', out)
        self.assertIn('"name":"Top","recv":"","line":8,"end":8,"exported":true', out)


class Mutate(unittest.TestCase):
    def test_c(self):
        self.assertEqual(add_param("int f(void);\n", 1, "f", "c"), "int f(double oracle_extra);\n")
        self.assertEqual(add_param("static int\nf(int a,\n  char *b)\n{", 2, "f", "c"), "static int\nf(int a,\n  char *b, double oracle_extra)\n{")
        with self.assertRaises(Skip):
            add_param("int f(const char *fmt, ...);", 1, "f", "c")

    def test_rust_generics_and_trailing_comma(self):
        self.assertEqual(add_param("fn f<F: Fn(u8) -> u8>(g: F,\n) {}", 1, "f", "rust"),
                         "fn f<F: Fn(u8) -> u8>(g: F,\n_oracle_extra: f64) {}")
        self.assertEqual(add_param("pub fn f(&self) {}", 1, "f", "rust"), "pub fn f(&self, _oracle_extra: f64) {}")

    def test_go_method(self):
        self.assertEqual(add_param("func (r *T) f() {}", 1, "f", "go"), "func (r *T) f(oracle_extra float64) {}")

    def test_restore_is_byte_exact_and_stays_in_the_repo(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d) / "repo"
            root.mkdir()
            raw = b"int f(int a)\r\n{ return a; } /* \xff\xfe */\r\n"
            (root / "a.c").write_bytes(raw)
            (Path(d) / "out.c").write_bytes(raw)
            with oracle.Mutation(root) as m:
                m.edit("a.c", 1, "f", "c")
                self.assertIn(b"double oracle_extra", (root / "a.c").read_bytes())
                with self.assertRaises(Skip):
                    m.edit("../out.c", 1, "f", "c")
            self.assertEqual((root / "a.c").read_bytes(), raw)

    def test_signal_restores(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "a.c").write_bytes(b"int f(int a);\n")
            with self.assertRaises(SystemExit) as e:
                with oracle.Mutation(root) as m:
                    m.edit("a.c", 1, "f", "c")
                    oracle.restore_on_signal(signal.SIGTERM, None)
            self.assertEqual(e.exception.code, 128 + signal.SIGTERM)
            self.assertEqual((root / "a.c").read_bytes(), b"int f(int a);\n")
            self.assertEqual(oracle.ACTIVE, [])

    def test_interrupt_cancels_queued_work(self):
        ran = []

        def work(i):
            ran.append(i)
            if i == 0:
                raise KeyboardInterrupt
            return i

        with self.assertRaises(KeyboardInterrupt):
            oracle.pool_map(work, range(200), 1)
        self.assertLess(len(ran), 200)


class CScan(unittest.TestCase):
    SRC = ("#include <x.h>\n"
           "static struct list **append(struct list **p,\n"
           "\t\t\tint x)\n"
           "{\n"
           "#ifdef A\n"
           "\tif (x) {\n"
           "#else\n"
           "\tif (!x) {\n"
           "#endif\n"
           "\t\treturn p;\n"
           "\t}\n"
           "\treturn \"}\" ? p : 0;\n"
           "}\n"
           "static const struct opt opts[] = { { 1 } };\n"
           "int main(void) { return 0; }\n")

    def test_functions_with_pointer_returns_and_branches(self):
        got = [(d.name, d.line, d.end, d.public) for d in scan.c_functions(self.SRC)]
        self.assertEqual(got, [("append", 2, 13, False), ("main", 15, 15, True)])

    def test_declarator_after_attributes_and_macro_uses(self):
        src = ("__attribute__((format (printf, 2, 3)))\nstatic void err(int s, const char *fmt, ...)\n{\n}\n"
               "static GIT_PATH_FUNC(git_path_x, \"x\")\n\nint (*getfn(void))(int)\n{\n}\n")
        self.assertEqual([(d.name, d.line) for d in scan.c_functions(src)], [("err", 2), ("getfn", 7)])

    def test_inactive_branch_from_preprocessed_output(self):
        active = scan.preprocessed_lines((DATA / "gcc16-t.i").read_text(), "/nonexistent")["t.c"]
        self.assertEqual(scan.inactive_lines((DATA / "t.c").read_text(), active), {20})

    def test_define_lines(self):
        src = "#define A(x) \\\n  f(x)\nint g;\n"
        self.assertEqual([scan.c_in_define(src, n) for n in (1, 2, 3)], [True, True, False])


class RustScan(unittest.TestCase):
    def test_fns_in_testdata(self):
        got = [(d.name, d.line, d.end, d.public, d.trait) for d in scan.rust_fns((DATA / "lib.rs").read_text())]
        self.assertEqual(got, [("target", 1, 3, True, False), ("users", 5, 10, True, False), ("S.m", 14, 16, True, False),
                               ("more", 25, 32, True, False), ("other", 34, 36, False, False)])

    def test_containers_skip_strings_and_comments(self):
        src = ("/// docs {\n#[cfg(test)]\nmod tests {\n    fn a() {\n        let s = \"\n}\n\";\n"
               "        let r = r#\"{\"#; let c = '{'; let l: &'static str = \"\";\n    }\n    #[test]\n    fn b() {}\n}\n"
               "impl<'a, T: Fn(u8) -> u8> fmt::Debug for Wrap<'a, T> {\n    fn fmt(&self) {}\n}\n"
               "impl<T> Wrap<T> {\n    pub(crate) fn c(&self) {}\n}\nfn r#async() {}\n")
        got = [(d.name, d.public, d.trait) for d in scan.rust_fns(src)]
        self.assertEqual(got, [("tests.a", False, False), ("tests.b", False, False), ("Wrap.fmt", False, True),
                               ("Wrap.c", False, False), ("r#async", False, False)])

    def test_cfg(self):
        self.assertTrue(scan.eval_cfg("unix"))
        self.assertFalse(scan.eval_cfg("windows"))
        self.assertTrue(scan.eval_cfg('all(unix, not(target_os = "macos"))'))
        self.assertFalse(scan.eval_cfg("not(any(windows, unix))"))
        src = "#[cfg(windows)]\nfn w() {\n    x();\n}\n#[cfg(unix)]\nfn u() {}\n#[cfg(not(unix))]\nuse a::b;\nfn z() {}\n"
        self.assertEqual(scan.rust_cfg_off(src), {1, 2, 3, 4, 7, 8})


class FoxText(unittest.TestCase):
    def test_callers_rows(self):
        g = foxcli.parse_graph(
            "12 lines name `x`: ...\ncallers of ~c:6f94 function x  strbuf.c:325\n3 call sites in 2 files\n"
            "abspath.c (2)\n  :126,180 realpath_1 c:1477\nbuiltin/\n am.c (1)\n  :1303 parse_mail c:1a6a guess by name | x(a);\n"
            "references, not calls (what is called through them is not followed):\n  git.c:534 | { x },\n~ tree-sitter\n")
        self.assertEqual(g.target, "strbuf.c:325")
        self.assertEqual([(s.path, s.line, s.who, s.guess) for s in g.sites], [
            ("abspath.c", 126, "realpath_1", False), ("abspath.c", 180, "realpath_1", False),
            ("builtin/am.c", 1303, "parse_mail", True)])

    def test_follows_cursor_pages(self):
        with tempfile.TemporaryDirectory() as d:
            fake = Path(d) / "fox"
            fake.write_text("#!/bin/sh\ncase \"$*\" in\n*--cursor\\ 2*) echo ' 9 method b c:2 ..9';;\n"
                            "*) printf '~outline a.rs\\n1 function a c:1 ..3\\n+1 more (answer cut) \\302\\267 cursor=2\\n';;\nesac\n")
            fake.chmod(0o755)
            o = foxcli.Fox(str(fake), d, d).outline("a.rs")
        self.assertEqual([(s.line, s.name) for s in o], [(1, "a"), (9, "b")])

    def test_refuses_live_store(self):
        with self.assertRaises(SystemExit):
            foxcli.Fox("fox", os.path.expanduser("~/.foxdb"), ".")


if __name__ == "__main__":
    unittest.main()
