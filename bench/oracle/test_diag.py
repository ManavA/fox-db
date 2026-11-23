"""Diagnostic parser and mutator tests on captured compiler output: python3 -m unittest bench/oracle/test_diag.py"""

import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import diag  # noqa: E402
import foxcli  # noqa: E402
from oracle import Skip, add_param, call_line, in_trait, qualify  # noqa: E402


class Gcc(unittest.TestCase):
    """testdata/gcc16-mutated.txt: gcc 16 on testdata/t.c after `target` gained a parameter."""

    def setUp(self):
        self.ds = diag.parse_gcc((HERE / "testdata/gcc16-mutated.txt").read_text())

    def sites(self):
        return [(*d.site(), d.func, diag.classify_c(d, "target")) for d in self.ds]

    def test_each_site_is_classified_and_named(self):
        self.assertEqual(self.sites(), [
            ("t.h", 3, "in_header", "call"),
            ("t.c", 5, None, "store"),
            ("t.c", 7, None, "store"),
            ("t.c", 9, "user1", "call"),
            ("t.c", 10, "user2", "store"),
            ("t.c", 10, "user2", "store"),
            ("t.c", 10, "user2", "call"),
            ("t.c", 11, "user3", "store"),
            ("t.c", 12, "user4", "other"),
            ("t.c", 13, None, "decl"),
        ])

    def test_macro_call_is_placed_at_its_use(self):
        d = self.ds[6]
        self.assertEqual((d.file, d.line), ("t.h", 2))
        self.assertEqual(d.site(), ("t.c", 10))

    def test_conflict_points_at_the_prototype(self):
        self.assertEqual(diag.conflicting_decls(self.ds, "target"), [("t.h", 1)])

    def test_ascii_quotes(self):
        ds = diag.parse_gcc("a.c: In function 'f':\na.c:3:9: error: too few arguments to function 'g'; expected 2, have 1\n")
        self.assertEqual([(d.func, diag.classify_c(d, "g")) for d in ds], [("f", "call")])


class Rustc(unittest.TestCase):
    def test_json_errors(self):
        ds = diag.parse_rustc_json((HERE / "testdata/rustc-mutated.jsonl").read_text())
        got = sorted({(d.file, d.line, d.code, diag.classify_rust(d, "target")) for d in ds})
        self.assertEqual(got, [("src/lib.rs", 6, "E0061", "call"), ("src/lib.rs", 7, "E0308", "store"),
                               ("src/lib.rs", 8, "E0593", "store"), ("src/lib.rs", 8, "E0599", "other"),
                               ("src/lib.rs", 9, "E0061", "call")])


class GoVet(unittest.TestCase):
    def test_vet_errors(self):
        ds = diag.parse_go((HERE / "testdata/go-mutated.txt").read_text())
        got = [(d.file, d.line, diag.classify_go(d, "target")) for d in ds]
        self.assertEqual(got, [("./t.go", 6, "call"), ("./t.go", 7, "store"), ("./t.go", 8, "store")])


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

    def test_outline_enclosing(self):
        o = foxcli.parse_outline("~outline a.rs\n3 struct Foo c:1 ..4\n 5 method bar c:2 ..9\n22 function baz c:3 ..30\n")
        self.assertEqual(foxcli.enclosing(o, 7).name, "bar")
        self.assertEqual(foxcli.enclosing(o, 25).name, "baz")
        self.assertIsNone(foxcli.enclosing(o, 21))


class Naming(unittest.TestCase):
    def test_rust_containers(self):
        src = ["impl<'a, T: Fn(u8) -> u8> Wrap<'a, T> {", "    fn a(&self) {}", "}", "impl fmt::Debug for Wrap<'_> {",
               "    fn fmt(&self) {}", "}", "mod tests {", "    fn b() {}", "}", "fn c() {}"]
        self.assertEqual([qualify("rust", src, n, x) for n, x in ((2, "a"), (5, "fmt"), (8, "b"), (10, "c"))],
                         ["Wrap.a", "Wrap.fmt", "tests.b", "c"])
        self.assertEqual([in_trait(src, n) for n in (2, 5, 8)], [False, True, False])

    def test_rust_container_skips_strings_and_comments(self):
        src = ["/// docs {", "#[cfg(test)]", "mod tests {", "    fn a() {", '        let s = "', "}", '";',
               "        let r = r#\"{\"#; let c = '{';", "    }", "    #[test]", "    fn b() {}", "}", "fn c() {}"]
        self.assertEqual([qualify("rust", src, n, x) for n, x in ((4, "a"), (11, "b"), (13, "c"))],
                         ["tests.a", "tests.b", "c"])

    def test_go_receiver(self):
        self.assertEqual(qualify("go", ["func (t *Terminal[T]) run() {"], 1, "run"), "Terminal.run")

    def test_go_call_line_is_the_call_start(self):
        src = b'func f() {\n\tx := target(1,\n\t\t"a)",\n\t)\n}\n'
        self.assertEqual(call_line(src, 4, 2, "target"), 2)


if __name__ == "__main__":
    unittest.main()
