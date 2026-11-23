"""Unit tests for bench/ab.py. No model calls: run with `python3 -m unittest discover -s bench -p 'test_*.py'`."""

import contextlib
import io
import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ab  # noqa: E402

TASK = {
    "id": "callers",
    "prompt": "p",
    "expect": ["cmd_find", {"name": "db::open_ro", "kind": "call"}, {"name": "tiers.rs", "kind": "doc"}],
    "universe": ["cli::cmd_find", "db::open_ro", "db::open_rw", "outbox::open", "indexstore::open",
                 "config::Config", "src/tiers.rs", "src/cli.rs", "src/db.rs"],
}


def task(expect, universe=TASK["universe"]):
    return {"id": "t", "prompt": "p", "expect": expect, "universe": universe}


class Tokens(unittest.TestCase):
    def test_paths_lose_line_numbers_and_calls_lose_parens(self):
        toks = ab.named_tokens("The caller is cmd_find in src/cli.rs:120, see `open_ro()` and Foo.")
        self.assertTrue({"cmd_find", "src/cli.rs", "open_ro", "Foo"} <= toks)
        self.assertNotIn("src/cli.rs:120", toks)


class Grade(unittest.TestCase):
    def test_items_accept_strings_and_tables(self):
        self.assertEqual(ab.expect_items(TASK), [("cmd_find", "any"), ("db::open_ro", "call"), ("tiers.rs", "doc")])

    def test_oracle_scores_one_everywhere(self):
        g = ab.grade(ab.oracle_answer(ab.expect_items(TASK)), TASK)
        self.assertEqual((g["precision"], g["recall"], g["f1"]), (1.0, 1.0, 1.0))
        self.assertEqual(set(g["by_kind"]), {"any", "call", "doc"})
        self.assertTrue(all(k["f1"] == 1.0 for k in g["by_kind"].values()))
        self.assertTrue(g["pass"])

    def test_empty_scores_zero(self):
        g = ab.grade("", TASK)
        self.assertEqual((g["precision"], g["recall"], g["f1"]), (0.0, 0.0, 0.0))
        self.assertFalse(g["pass"])

    def test_only_unexpected_repo_names_cost_precision(self):
        ans = ("cmd_find, db::open_ro and tiers.rs; also open_rw and `Config`. GitHub, read/write, and/or, "
               "https://github.com/x, macOS, src/cli.rs:120")
        g = ab.grade(ans, TASK)
        self.assertEqual(g["recall"], 1.0)
        self.assertAlmostEqual(g["precision"], 3 / 5)
        self.assertEqual(g["by_kind"]["doc"]["precision"], 1.0)
        self.assertAlmostEqual(g["by_kind"]["call"]["precision"], 1 / 3)

    def test_file_items_make_repo_path_citations_count(self):
        t = task([{"name": "src/tiers.rs", "kind": "file"}])
        g = ab.grade("See src/tiers.rs:3, src/cli.rs:120 and docs/x.md.", t)
        self.assertEqual(g["recall"], 1.0)
        self.assertAlmostEqual(g["precision"], 1 / 2)

    def test_qualified_item_matches_its_last_segment(self):
        self.assertEqual(ab.grade("It is open_ro.", task(["db::open_ro"]))["recall"], 1.0)
        self.assertEqual(ab.grade("crate::db::open_ro", task(["db::open_ro"]))["recall"], 1.0)

    def test_qualified_token_needs_a_unique_last_segment(self):
        self.assertEqual(ab.grade("std::fs::File::open", task(["open"]))["recall"], 0.0)
        self.assertEqual(ab.grade("open", task(["open"]))["recall"], 1.0)
        self.assertEqual(ab.grade("outbox::open", task(["open"], ["outbox::open"]))["recall"], 1.0)

    def test_variants_of_one_item_count_once(self):
        g = ab.grade("open_ro, db::open_ro and crate::db::open_ro", task(["db::open_ro"]))
        self.assertEqual((g["precision"], g["recall"]), (1.0, 1.0))

    def test_bare_words_are_found(self):
        self.assertEqual(ab.grade("It returns a Config value.", task(["Config"]))["recall"], 1.0)

    def test_plain_words_count_against_precision_only_when_marked_as_code(self):
        t = task(["cmd_find"], TASK["universe"] + ["search::run", "db::store", "cli::status"])
        self.assertEqual(ab.grade("cmd_find opens the store to run a search; see status.", t)["precision"], 1.0)
        self.assertAlmostEqual(ab.grade("cmd_find calls `run` and status()", t)["precision"], 1 / 3)

    def test_names_outside_the_universe_are_ignored(self):
        g = ab.grade("cmd_find and some_helper and Foo::bar", task(["cmd_find"]))
        self.assertEqual(g["precision"], 1.0)

    def test_check_only_task_keeps_substring_rule(self):
        t = {"id": "x", "prompt": "p", "check": "tiers.rs"}
        self.assertEqual(ab.grade("see src/tiers.rs", t), {"pass": True})
        self.assertEqual(ab.grade("no idea", t), {"pass": False})

    def test_task_without_check_or_expect_has_no_verdict(self):
        self.assertEqual(ab.grade("anything", {"id": "x", "prompt": "p"}), {"pass": None})

    def test_universe_from_store(self):
        with tempfile.TemporaryDirectory() as d:
            db = Path(d) / "fox.db"
            c = sqlite3.connect(db)
            c.execute("create table node(kind text, name text, path text)")
            c.executemany("insert into node values (?, ?, ?)",
                          [("symbol", "open_ro", "src/db.rs"), ("file", "db.rs", "src/db.rs"),
                           ("doc_section", "Intro", "README.md")])
            c.commit()
            c.close()
            self.assertEqual(ab.store_universe(db), {"db::open_ro", "src/db.rs"})


class SelfTest(unittest.TestCase):
    def test_decoys_are_unique_and_wrong(self):
        items = ab.expect_items(TASK)
        ds = ab.decoys(items, 50, seed=1)
        self.assertEqual(len(set(ds)), 50)
        g = ab.grade("\n".join(ds), {**TASK, "universe": TASK["universe"] + ds})
        self.assertEqual(g["recall"], 0.0)

    def test_oracles_are_plain_prose(self):
        items = ab.expect_items(TASK)
        for ans in ab.oracle_answers(items, TASK["universe"]):
            self.assertNotIn("`", ans)
        self.assertEqual(len(ab.oracle_answers(items, TASK["universe"])), 2)
        self.assertNotEqual(*ab.oracle_answers(items, TASK["universe"]))

    def test_selftest_passes_on_a_sound_task(self):
        ok, lines = ab.grader_selftest({"task": [TASK, {"id": "old", "prompt": "p", "check": "x"}]})
        self.assertTrue(ok, lines)
        self.assertTrue(any("old" in ln and "skip" in ln for ln in lines))

    def test_selftest_fails_when_an_item_cannot_be_named(self):
        ok, lines = ab.grader_selftest({"task": [{"id": "bad", "prompt": "p", "expect": ["two words"]}]})
        self.assertFalse(ok)
        self.assertTrue(any("bad" in ln and "FAIL" in ln for ln in lines))


class Schedule(unittest.TestCase):
    def test_pairs_are_adjacent_and_seeded(self):
        runs = ab.schedule(["a", "b", "c"], ["claude", "codex"], 3, seed=7)
        self.assertEqual(len(runs), 3 * 3 * 2 * 2)
        for first, second in zip(runs[::2], runs[1::2]):
            self.assertEqual((first["rep"], first["task"], first["harness"]),
                             (second["rep"], second["task"], second["harness"]))
            self.assertEqual({first["arm"], second["arm"]}, {"on", "off"})
        self.assertEqual(runs, ab.schedule(["a", "b", "c"], ["claude", "codex"], 3, seed=7))
        self.assertEqual({r["arm"] for r in runs[::2]}, {"on", "off"})

    def test_local_runs_three_arms_back_to_back(self):
        runs = ab.schedule(["a", "b"], ["local"], 2, seed=1)
        self.assertEqual(len(runs), 2 * 2 * 3)
        for i in range(0, len(runs), 3):
            group = runs[i:i + 3]
            self.assertEqual(len({(r["rep"], r["task"]) for r in group}), 1)
            self.assertEqual({r["arm"] for r in group}, {"grep", "fox-cli", "packet"})

    def test_default_repeat_is_three(self):
        self.assertEqual(ab.run_parser().parse_args(["t.toml"]).repeat, 3)
        self.assertEqual(ab.run_parser().parse_args(["t.toml"]).store, "scratch")


class Store(unittest.TestCase):
    def test_scratch_store_indexes_only_the_clone(self):
        with tempfile.TemporaryDirectory() as d:
            work = Path(d)
            plan = ab.store_plan("scratch", "/x/fox", work / "clone", work / "run-home")
            self.assertEqual(plan["index"], ["/x/fox", "index", str(work / "clone"), "--quiet", "--no-global"])
            self.assertEqual(plan["env"]["FOXDB_HOME"], str(work / "run-home"))
            self.assertTrue(plan["env"]["FOXDB_CLAUDE_PROJECTS"].startswith(str(work / "run-home")))

    def test_live_store_changes_nothing(self):
        self.assertEqual(ab.store_plan("live", "/x/fox", Path("/c"), Path("/h")), {"index": None, "env": {}})

    def test_scratch_home_inside_live_store_is_refused(self):
        with self.assertRaises(SystemExit):
            ab.store_plan("scratch", "fox", Path("/c"), Path.home() / ".foxdb" / "ab" / "h")

    def test_fresh_home_wipes_a_reused_store(self):
        with tempfile.TemporaryDirectory() as d:
            home = Path(d) / "run.home"
            (home / "claude-projects").mkdir(parents=True)
            (home / "fox.db").write_text("old")
            ab.fresh_home(home)
            self.assertEqual([p.name for p in home.iterdir()], ["claude-projects"])
        with self.assertRaises(SystemExit):
            ab.fresh_home(Path.home() / ".foxdb")

    def test_mcp_servers_get_the_store_env(self):
        env = {"FOXDB_HOME": "/s/h", "FOXDB_CLAUDE_PROJECTS": "/s/h/cp"}
        cfg = ab.mcp_config_json("/x/fox", True, env)
        self.assertEqual(cfg["mcpServers"]["foxdb"]["env"], env)
        self.assertEqual(ab.mcp_config_json("/x/fox", False, env), {"mcpServers": {}})
        cmd = ab.codex_cmd("q", Path("/c"), "/x/fox", True, Path("/l"), env, None)
        self.assertIn('env={FOXDB_HOME="/s/h",FOXDB_CLAUDE_PROJECTS="/s/h/cp"}', " ".join(cmd))


class Provenance(unittest.TestCase):
    def test_sha256_of_file_and_missing_file(self):
        with tempfile.NamedTemporaryFile() as f:
            f.write(b"abc")
            f.flush()
            self.assertEqual(ab.sha256_file(f.name),
                             "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        self.assertIsNone(ab.sha256_file("/nonexistent/fox"))

    def test_claude_model_comes_from_model_usage(self):
        out = json.dumps({"total_cost_usd": 0.1, "num_turns": 2, "result": "a",
                          "modelUsage": {"claude-x": {}, "claude-y": {}}})
        self.assertEqual(ab.usage_from_claude(out)["model"], "claude-x,claude-y")


class Stats(unittest.TestCase):
    def test_pass_hat_k(self):
        self.assertEqual(ab.pass_hat_k(3, 3, 3), 1.0)
        self.assertEqual(ab.pass_hat_k(3, 2, 3), 0.0)
        self.assertAlmostEqual(ab.pass_hat_k(4, 2, 2), 1 / 6)
        self.assertAlmostEqual(ab.pass_hat_k(4, 2, 1), 0.5)

    def test_percentile_interpolates(self):
        self.assertAlmostEqual(ab.percentile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 90), 9.1)
        self.assertEqual(ab.percentile([4.0], 90), 4.0)

    def test_bootstrap_is_seeded_and_brackets_the_mean(self):
        xs = [0.1, -0.2, 0.3, 0.05, 0.0]
        lo, hi = ab.bootstrap_ci(xs, seed=3, draws=2000)
        self.assertEqual((lo, hi), ab.bootstrap_ci(xs, seed=3, draws=2000))
        self.assertLessEqual(lo, sum(xs) / len(xs))
        self.assertGreaterEqual(hi, sum(xs) / len(xs))
        self.assertEqual(ab.bootstrap_ci([2.0], seed=0, draws=100), (2.0, 2.0))

    def rows(self):
        out = []
        for task, d in (("a", 0.1), ("b", 0.3)):
            for rep in range(3):
                for arm, cost, f1, ok in (("on", 1.0, 1.0, True), ("off", 1.0 + d, 0.5, rep != 0)):
                    out.append({"task": task, "harness": "claude", "foxdb": arm, "rep": rep, "headroom": "headroom-off",
                                "store": "scratch", "cost_usd": cost, "turns": 4 if arm == "on" else 6,
                                "f1": f1, "pass": ok})
        return out

    def test_report_pairs_on_off_per_task_and_overall(self):
        text = ab.report(self.rows(), seed=0, draws=1000)
        self.assertIn("claude · headroom-off · store scratch", text)
        lines = {ln.split()[0]: ln for ln in text.splitlines() if ln.split() and ln.split()[0] in ("a", "b", "ALL")}
        self.assertIn("cost -0.1000", lines["a"])
        self.assertIn("cost -0.2000", lines["ALL"])
        self.assertIn("turns -2.00", lines["ALL"])
        self.assertIn("f1 +0.500", lines["ALL"])
        self.assertIn("pass^3 on 1.00 off 0.00", lines["ALL"])
        self.assertIn("p90 cost on 1.0000", text)

    def test_report_keeps_runs_apart(self):
        rows = []
        for seed, cost in ((1, 1.0), (2, 5.0)):
            for arm in ("on", "off"):
                rows.append({"task": "a", "harness": "claude", "foxdb": arm, "rep": 0, "headroom": "headroom-off",
                             "store": "scratch", "seed": seed, "model_requested": "default",
                             "harness_version": "2.0", "cost_usd": cost + (0 if arm == "on" else 1), "pass": True})
        text = ab.report(rows, seed=0, draws=100)
        self.assertEqual(text.count("store scratch"), 2)
        self.assertIn("seed 1", text)
        self.assertIn("seed 2", text)
        self.assertEqual(text.count("cost -1.0000"), 4)

    def test_report_compares_local_arms_with_grep(self):
        rows = [{"task": "a", "harness": "local", "foxdb": arm, "rep": rep, "turns": turns, "pass": ok, "loop": False}
                for rep in range(2)
                for arm, turns, ok in (("grep", 10, False), ("fox-cli", 6, True), ("packet", 4, True))]
        text = ab.report(rows, seed=0, draws=100)
        self.assertIn("· fox-cli minus grep", text)
        self.assertIn("· packet minus grep", text)
        self.assertNotIn("minus packet", text)
        self.assertIn("turns -4.00", text)
        self.assertIn("turns -6.00", text)
        self.assertIn("pass^2 packet 1.00 grep 0.00", text)

    def test_local_usage_comes_from_the_last_json_line(self):
        out = "noise\n" + json.dumps({"answer": "x", "steps": 5, "input": 900, "output": 80, "cap": "steps",
                                      "loop": True, "max_repeat": 3, "secs": 12.0,
                                      "server": {"model": "m.gguf", "ctx": 16384}})
        u = ab.usage_from_local(out)
        self.assertEqual((u["turns"], u["input"], u["cap"], u["loop"], u["model"], u["ctx"], u["answer"]),
                         (5, 900, "steps", True, "m.gguf", 16384, "x"))
        self.assertEqual(ab.usage_from_local(""), {"parse_error": True})

    def test_report_handles_old_rows(self):
        rows = [{"task": "a", "harness": "codex", "foxdb": arm, "rep": 0, "headroom": "headroom-off",
                 "turns": 3, "pass": None} for arm in ("on", "off")]
        text = ab.report(rows, seed=0, draws=100)
        self.assertIn("store unknown", text)
        self.assertIn("turns +0.00", text)


class DryRun(unittest.TestCase):
    def test_dry_run_prints_plan_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            tasks = Path(d) / "t.toml"
            tasks.write_text('repo = "/r"\n[[task]]\nid = "a"\nprompt = "q"\nexpect = ["x_y"]\n')
            out = Path(d) / "res.jsonl"
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                rc = ab.main([str(tasks), "--harness", "claude", "--seed", "5", "--work", str(Path(d) / "w"),
                              "--out", str(out), "--fox", "/nonexistent/fox"])
            self.assertEqual(rc, 0)
            text = buf.getvalue()
            self.assertIn("DRY RUN", text)
            self.assertIn("seed 5", text)
            self.assertIn("index", text)
            self.assertIn("--no-global", text)
            self.assertIn("grader.home", text)
            self.assertFalse(out.exists())

    def test_local_dry_run_indexes_only_fox_arms(self):
        with tempfile.TemporaryDirectory() as d:
            tasks = Path(d) / "t.toml"
            tasks.write_text('repo = "/r"\n[[task]]\nid = "a"\nprompt = "q"\n')
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                ab.main([str(tasks), "--harness", "local", "--seed", "5", "--repeat", "1", "--work",
                         str(Path(d) / "w"), "--fox", "/nonexistent/fox"])
            slots = buf.getvalue().split("# slot ")[1:]
            self.assertEqual(len(slots), 3)
            for s in slots:
                arm = s.split()[1].split("-", 2)[2].rsplit("-", 1)[0]
                self.assertIn(f"--arm {arm}", s)
                self.assertIn("local_agent.py", s)
                self.assertEqual("--no-global" in s, arm != "grep")


if __name__ == "__main__":
    unittest.main()
