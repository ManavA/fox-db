"""Depth-stratified oracle tasks: python3 -m unittest bench/oracle/test_depth.py"""

import sys
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import foxcli  # noqa: E402
import oracle  # noqa: E402
import scan  # noqa: E402
from oracle import Skip  # noqa: E402

IMPACT = """22 lines name `opt`: 21 uses (d1), 1 definition
impact of ~c:8357 function opt  th.c:331: 5 symbols, 1 files at top level; per depth 3/2
d1
 th.c
  :371 standard_options c:8358 | :376 opt(t, "progress");
  :720 fetch_refs c:8368 guess 1 of 5 | :760 opt(transport, "cloning");
  :1346 (top) | .set_option	= opt,
 transport.c
  :1319 transport_set_option c:83dd via transport_vtable.set_option | int transport_set_option(
d2
 builtin/clone.c
  :917 cmd_clone c:1c97 guess (via a guessed caller) | int cmd_clone(int argc,
unresolved mentions:
th.c:9 guessed elsewhere | opt()
~ tree-sitter, name-resolved
"""


def site(path, line, func, kind="call"):
    return {"path": path, "line": line, "kind": kind, "func": func, "msg": "", "code": ""}


class Impact(unittest.TestCase):
    def test_rows_carry_depth_guess_and_pointer_hops(self):
        g = foxcli.parse_impact(IMPACT)
        self.assertEqual(g.target, "th.c:331")
        self.assertEqual([(r.depth, r.path, r.line, r.who, r.guess, r.via) for r in g.rows], [
            (1, "th.c", 371, "standard_options", False, False),
            (1, "th.c", 720, "fetch_refs", True, False),
            (1, "th.c", 1346, "(top)", False, False),
            (1, "transport.c", 1319, "transport_set_option", False, True),
            (2, "builtin/clone.c", 917, "cmd_clone", True, False)])


class Labels(unittest.TestCase):
    def test_certain_beats_guess_and_absent_is_missing(self):
        gold = {("a.c", "f"), ("a.c", "g"), ("b.c", "h")}
        said = [("a.c", "f", True), ("a.c", "f", False), ("a.c", "g", True), ("c.c", "x", False),
                ("c.c", "y", True), ("d.c", "z", False)]
        labels, extra = oracle.label_items(gold, said, via={("d.c", "z")})
        self.assertEqual(labels, {("a.c", "f"): "certain", ("a.c", "g"): "guess", ("b.c", "h"): "missing"})
        self.assertEqual(extra, [{"path": "c.c", "func": "x", "fox": "certain", "via": False},
                                 {"path": "c.c", "func": "y", "fox": "guess", "via": False},
                                 {"path": "d.c", "func": "z", "fox": "certain", "via": True}])

    def test_top_level_rows_are_not_functions(self):
        labels, extra = oracle.label_items(set(), [("a.c", "(top)", False)])
        self.assertEqual((labels, extra), ({}, []))


class StubBackend:
    def __init__(self, lang, defs):
        self.lang, self.defs = lang, defs

    def defs_in(self, rel):
        return self.defs.get(rel, [])


def target(sites):
    return {"name": "t", "path": "a.c", "line": 1, "bare": "t", "sites": sites}


class Hop1(unittest.TestCase):
    def test_each_caller_resolves_to_its_one_definition(self):
        be = StubBackend("c", {"a.c": [scan.Def("t", "t", 1, 3), scan.Def("f", "f", 5, 9)],
                               "b.c": [scan.Def("g", "g", 2, 8)]})
        r = target([site("a.c", 6, "f"), site("a.c", 7, "f"), site("b.c", 4, "g"), site("b.c", 5, "x", "store")])
        self.assertEqual(oracle.hop1_defs(be, r, 10), [
            {"name": "f", "path": "a.c", "line": 5, "bare": "f"}, {"name": "g", "path": "b.c", "line": 2, "bare": "g"}])

    def test_unresolvable_callers_skip_the_task(self):
        be = StubBackend("c", {"a.c": [scan.Def("f", "f", 5, 9), scan.Def("f", "f", 12, 20)]})
        with self.assertRaises(Skip):
            oracle.hop1_defs(be, target([site("a.c", 6, "f")]), 10)
        with self.assertRaises(Skip):
            oracle.hop1_defs(be, target([site("b.c", 6, "nowhere")]), 10)

    def test_trait_methods_and_too_many_callers_skip(self):
        be = StubBackend("rust", {"a.rs": [scan.Def("S.write", "write", 5, 9, trait=True)]})
        with self.assertRaises(Skip):
            oracle.hop1_defs(be, target([site("a.rs", 6, "S.write")]), 10)
        be = StubBackend("c", {"a.c": [scan.Def(f"f{i}", f"f{i}", 10 * i, 10 * i + 5) for i in range(1, 4)]})
        with self.assertRaises(Skip):
            oracle.hop1_defs(be, target([site("a.c", 10 * i + 1, f"f{i}") for i in range(1, 4)]), 2)


class Gold(unittest.TestCase):
    def test_two_hops_of_calls_keep_the_nearer_hop_and_drop_the_target(self):
        r = target([site("a.c", 6, "f"), site("b.c", 4, "g")])
        callers = {("a.c", "f"): [site("b.c", 3, "g"), site("c.c", 2, "h"), site("c.c", 9, "k", "store")],
                   ("b.c", "g"): [site("a.c", 2, "t"), site("c.c", 5, "h"), site("d.c", 1, "m")]}
        self.assertEqual(oracle.impact_gold(r, callers), [
            {"func": "f", "path": "a.c", "hop": 1}, {"func": "g", "path": "b.c", "hop": 1},
            {"func": "h", "path": "c.c", "hop": 2}, {"func": "m", "path": "d.c", "hop": 2}])

    def test_a_task_needs_a_second_hop_a_size_cap_and_names_a_grader_can_tell_apart(self):
        one = {"func": "f", "path": "a.c", "hop": 1}
        self.assertEqual(oracle.depth2_skip([one], 9), "no function two hops out")
        self.assertEqual(oracle.depth2_skip([one, {"func": "g", "path": "b.c", "hop": 2}], 1), "2 functions, over 1")
        self.assertEqual(oracle.depth2_skip([one, {"func": "f", "path": "b.c", "hop": 2}], 9),
                         "a.c and b.c both define f; grading by name cannot tell them apart")
        self.assertIsNone(oracle.depth2_skip([one, {"func": "g", "path": "b.c", "hop": 2}], 9))


class Tasks(unittest.TestCase):
    def test_depth_hops_and_fox_labels_reach_the_task_file(self):
        r = dict(target([site("a.c", 6, "f"), site("b.c", 4, "g"), site("c.c", 9, "tbl", "store")]), stores=1)
        imp = {"name": "t", "path": "a.c", "line": 1, "bare": "t",
               "hops": [{"func": "f", "path": "a.c", "hop": 1}, {"func": "h", "path": "c.c", "hop": 2}]}
        labels = {"oracle-callers-t": ({("a.c", "f"): "certain", ("b.c", "g"): "guess"},
                                       [{"path": "z.c", "func": "zz", "fox": "certain", "via": False}]),
                  "oracle-impact2-t": ({("a.c", "f"): "certain", ("c.c", "h"): "missing"}, [])}
        spec = tomllib.loads(oracle.tasks_toml([r], "~/src/x", "cfg", [imp], labels))
        by = {t["id"]: t for t in spec["task"]}
        self.assertEqual(sorted(by), ["oracle-callers-t", "oracle-impact2-t", "oracle-stores-t"])
        self.assertEqual([by[i]["depth"] for i in sorted(by)], [1, 2, 1])
        self.assertEqual(by["oracle-callers-t"]["expect"], [
            {"name": "f", "file": "a.c", "kind": "call", "hop": 1, "fox": "certain"},
            {"name": "g", "file": "b.c", "kind": "call", "hop": 1, "fox": "guess"}])
        self.assertEqual(by["oracle-callers-t"]["fox_extra"], [{"name": "zz", "file": "z.c", "fox": "certain", "via": False}])
        self.assertEqual(by["oracle-impact2-t"]["expect"], [
            {"name": "f", "file": "a.c", "kind": "call", "hop": 1, "fox": "certain"},
            {"name": "h", "file": "c.c", "kind": "call", "hop": 2, "fox": "missing"}])
        self.assertIn("calls one of those callers", by["oracle-impact2-t"]["prompt"])
        self.assertNotIn("fox", by["oracle-stores-t"]["expect"][0])


if __name__ == "__main__":
    unittest.main()
