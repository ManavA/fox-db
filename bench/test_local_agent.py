"""Unit tests for bench/local_agent.py against a fake chat server. No model:
`python3 -m unittest discover -s bench -p 'test_*.py'`."""

import json
import os
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import local_agent as la  # noqa: E402


class FakeServer:
    """Replies with the scripted strings in order; a dict entry is sent as (status, body)."""

    def __init__(self, replies):
        self.replies, self.requests = list(replies), []
        outer = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["content-length"])))
                outer.requests.append(body)
                reply = outer.replies.pop(0) if outer.replies else "FINAL: out of script"
                if isinstance(reply, dict):
                    status, payload = reply["status"], reply["body"]
                else:
                    status = 200
                    payload = {"choices": [{"message": {"content": reply}, "finish_reason": "stop"}],
                               "usage": {"prompt_tokens": 100, "completion_tokens": 10}}
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class Parse(unittest.TestCase):
    def test_bash_block_is_the_action(self):
        self.assertEqual(la.parse_action("Look.\n```bash\ngrep -rn foo src\n```\n"), ("bash", "grep -rn foo src"))

    def test_bare_fence_counts_as_bash(self):
        self.assertEqual(la.parse_action("```\nls\n```"), ("bash", "ls"))

    def test_non_shell_fence_is_skipped(self):
        text = "```rust\nfn x() {}\n```\nnow\n```sh\nrg -n x\n```"
        self.assertEqual(la.parse_action(text), ("bash", "rg -n x"))

    def test_only_the_first_command_runs(self):
        self.assertEqual(la.parse_action("```bash\nls\n```\n```bash\npwd\n```"), ("bash", "ls"))

    def test_final_answer(self):
        self.assertEqual(la.parse_action("Done.\nFINAL: cmd_find in src/cli.rs:413"),
                         ("final", "cmd_find in src/cli.rs:413"))

    def test_final_keeps_code_after_it(self):
        kind, ans = la.parse_action("FINAL: callers are\n```bash\nfoo\n```")
        self.assertEqual(kind, "final")
        self.assertIn("foo", ans)

    def test_command_before_final_wins(self):
        self.assertEqual(la.parse_action("```bash\nls\n```\nFINAL: guess"), ("bash", "ls"))

    def test_markdown_final(self):
        self.assertEqual(la.parse_action("**FINAL:** a, b"), ("final", "a, b"))

    def test_think_block_is_ignored(self):
        self.assertEqual(la.parse_action("<think>FINAL: no</think>```bash\nls\n```"), ("bash", "ls"))

    def test_no_action_is_a_format_error(self):
        self.assertEqual(la.parse_action("I will grep for it."), ("error", la.FORMAT_ERROR))
        self.assertEqual(la.parse_action("```bash\n\n```"), ("error", la.FORMAT_ERROR))


class Cap(unittest.TestCase):
    def test_short_output_is_unchanged(self):
        self.assertEqual(la.cap_output("a\nb", 2000), "a\nb")

    def test_long_output_is_cut_on_a_line_with_a_count(self):
        text = "\n".join(f"line {i:04d}" for i in range(1000))
        out = la.cap_output(text, 2000)
        kept = out.splitlines()[:-1]
        self.assertLessEqual(len("\n".join(kept)), 2000)
        self.assertEqual(kept[-1], f"line {len(kept) - 1:04d}")
        self.assertEqual(out.splitlines()[-1], f"[{1000 - len(kept)} more lines; narrow your command]")

    def test_one_huge_line_is_cut(self):
        out = la.cap_output("x" * 50_000, 2000)
        self.assertEqual(out, "x" * la.LINE_CAP + " [line cut]")

    def test_run_bash_reports_exit_and_empty_output(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(la.run_bash("true", Path(d), dict(os.environ)), "[no output]")
            self.assertEqual(la.run_bash("echo hi; exit 3", Path(d), dict(os.environ)), "hi\n[exit 3]")
            self.assertIn("[timed out after 1s]", la.run_bash("sleep 5", Path(d), dict(os.environ), timeout=1))


class Arms(unittest.TestCase):
    def test_prompt_names(self):
        self.assertEqual(la.prompt_names("Who calls db::open_ro and `Graph.load`? Also is_secret_name, SaveLedger."),
                         ["db::open_ro", "Graph.load", "is_secret_name", "SaveLedger"])
        self.assertEqual(la.prompt_names("Where does this repo decide a file is secret?"), [])

    def test_fox_guide_only_in_fox_arm(self):
        self.assertIn("fox code", la.system_prompt("fox-cli", 20))
        self.assertNotIn("fox", la.system_prompt("grep", 20))
        self.assertEqual(la.system_prompt("grep", 20), la.system_prompt("packet", 20))
        self.assertLess(len(la.system_prompt("fox-cli", 20)) / 4, 600)

    def test_env_puts_fox_on_path_only_for_fox_cli(self):
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            fox_dir = d / "foxbin"
            fox_dir.mkdir()
            (fox_dir / "fox").write_text("#!/bin/sh\necho fox\n")
            (fox_dir / "fox").chmod(0o755)
            base = {"PATH": f"{fox_dir}:/usr/bin:/bin", "FOXDB_HOME": str(d / "store")}
            grep = la.agent_env("grep", str(fox_dir / "fox"), d / "g", base)
            self.assertNotIn(str(fox_dir), grep["PATH"])
            self.assertEqual(grep["HOME"], str(d / "g" / "home"))
            self.assertNotEqual(grep["FOXDB_HOME"], str(d / "store"))
            self.assertIn("[exit 127]", la.run_bash("fox", d, grep))
            fx = la.agent_env("fox-cli", str(fox_dir / "fox"), d / "f", base)
            self.assertEqual(fx["FOXDB_HOME"], str(d / "store"))
            self.assertEqual(la.run_bash("fox", d, fx), "fox")


class Loop(unittest.TestCase):
    def run_script(self, replies, **kw):
        srv = FakeServer(replies)
        self.addCleanup(srv.close)
        d = Path(tempfile.mkdtemp())
        (d / "a.txt").write_text("needle\n")
        log = d / "t.jsonl"
        res = la.run_agent("q", "grep", d, dict(os.environ), srv.url, transcript=log, **kw)
        return res, srv, [json.loads(ln) for ln in log.read_text().splitlines()]

    def test_command_then_final(self):
        res, srv, log = self.run_script(["```bash\ncat a.txt\n```", "FINAL: needle"], packet="PKT")
        self.assertEqual((res["answer"], res["steps"], res["input"], res["output"]), ("needle", 2, 200, 20))
        self.assertIsNone(res["cap"])
        self.assertFalse(res["loop"])
        self.assertEqual(srv.requests[0]["temperature"], 0)
        self.assertIn("PKT", srv.requests[0]["messages"][1]["content"])
        self.assertEqual(srv.requests[1]["messages"][-1], {"role": "user", "content": "needle"})
        self.assertEqual(log[-1]["role"], "result")

    def test_repeated_command_is_a_loop(self):
        res, _, _ = self.run_script(["```bash\nls\n```"] * 3 + ["```bash\npwd\n```", "FINAL: x"])
        self.assertEqual((res["loop"], res["loop_commands"], res["max_repeat"], res["commands"]), (True, 1, 3, 4))

    def test_step_cap_gets_one_last_call(self):
        res, srv, _ = self.run_script(["```bash\nls\n```"] * 3 + ["FINAL: late"], steps=3)
        self.assertEqual((res["cap"], res["forced_final"], res["answer"], res["steps"]), ("steps", True, "late", 4))
        self.assertEqual(srv.requests[-1]["messages"][-1]["content"], la.LAST_CALL)

    def test_token_cap(self):
        res, _, _ = self.run_script(["```bash\nls\n```", "FINAL: t"], token_cap=50)
        self.assertEqual((res["cap"], res["steps"], res["answer"]), ("tokens", 2, "t"))

    def test_unparsed_reply_gets_a_format_error(self):
        res, srv, _ = self.run_script(["thinking...", "FINAL: ok"])
        self.assertEqual(srv.requests[1]["messages"][-1]["content"], la.FORMAT_ERROR)
        self.assertEqual((res["commands"], res["format_errors"]), (0, 1))

    def test_full_context_elides_old_outputs_for_a_last_call(self):
        full = {"status": 400, "body": {"error": {"message": "the request exceeds the available context size"}}}
        res, srv, _ = self.run_script(["```bash\necho one\n```", "```bash\necho two\n```", "```bash\necho three\n```",
                                       full, "FINAL: squeezed"])
        self.assertEqual((res["cap"], res["answer"]), ("context", "squeezed"))
        msgs = srv.requests[-1]["messages"]
        self.assertEqual(msgs[3]["content"], "[output elided]")
        self.assertEqual([m["content"] for m in msgs[-4:]], ["two", "```bash\necho three\n```", "three", la.LAST_CALL])


if __name__ == "__main__":
    unittest.main()
