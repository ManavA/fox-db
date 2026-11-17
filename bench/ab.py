#!/usr/bin/env python3
"""Scripted agent A/B runner: the same task list with foxdb on and off, through `claude -p` and
`codex exec`.

Dry-run by default: it prints every command it would run and runs nothing. Pass --run to execute;
that spends real model usage, so the controller decides when.

Each task runs in a throwaway copy of the repo (`git clone --shared` into --work), never in the
original. foxdb "on" means the `fox mcp` server is the only MCP server; "off" means no MCP servers. User
settings are skipped for claude (`--setting-sources project,local`) so a user-level foxdb hook
cannot leak into the "off" arm.
Headroom is per process only: --headroom on sets ANTHROPIC_BASE_URL to the local proxy for the
child processes, off removes it. Results go to --out as JSON lines, one per (task, harness, arm),
each carrying the headroom mode so numbers from different modes are never mixed.

Task file (TOML):
    repo = "~/foxdb"
    [[task]]
    id = "callers-open-ro"
    prompt = "List every function that calls open_ro and say what each uses it for."
    check = "open_rw"        # optional substring the final answer must contain
"""

import argparse
import json
import os
import shlex
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

HEADROOM_URL = "http://127.0.0.1:8787"


def env_for(headroom: str) -> dict:
    env = dict(os.environ)
    if headroom == "on":
        env["ANTHROPIC_BASE_URL"] = HEADROOM_URL
    else:
        env.pop("ANTHROPIC_BASE_URL", None)
    return env


def mcp_config(work: Path, fox: str, on: bool) -> Path:
    cfg = {"mcpServers": {"foxdb": {"command": fox, "args": ["mcp"]}}} if on else {"mcpServers": {}}
    p = work / ("mcp-on.json" if on else "mcp-off.json")
    p.write_text(json.dumps(cfg))
    return p


def claude_cmd(prompt: str, cfg: Path, max_usd: float) -> list:
    return ["claude", "-p", prompt, "--output-format", "json", "--strict-mcp-config", "--mcp-config", str(cfg),
            "--no-session-persistence", "--max-budget-usd", str(max_usd),
            # user-level hooks (a foxdb SessionStart brief) would leak into the "off" arm
            "--setting-sources", "project,local"]


def codex_cmd(prompt: str, cwd: Path, fox: str, on: bool, last: Path) -> list:
    cmd = ["codex", "exec", "--json", "--ephemeral", "--skip-git-repo-check", "-s", "read-only", "-C", str(cwd),
           "-o", str(last)]
    if on:
        cmd += ["-c", f'mcp_servers={{foxdb={{command="{fox}",args=["mcp"]}}}}']
    else:
        cmd += ["-c", "mcp_servers={}"]  # drop any user-configured foxdb server
    return cmd + [prompt]


def usage_from_claude(stdout: str) -> dict:
    try:
        j = json.loads(stdout)
    except json.JSONDecodeError:
        return {"parse_error": True}
    u = j.get("usage", {})
    return {"cost_usd": j.get("total_cost_usd"), "turns": j.get("num_turns"), "input": u.get("input_tokens"),
            "cache_read": u.get("cache_read_input_tokens"), "cache_write": u.get("cache_creation_input_tokens"),
            "output": u.get("output_tokens"), "answer": j.get("result", "")}


def usage_from_codex(stdout: str, last: Path) -> dict:
    tot = {"input": 0, "cache_read": 0, "output": 0, "turns": 0}
    for line in stdout.splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "turn.completed":
            u = ev.get("usage", {})
            tot["turns"] += 1
            tot["input"] += u.get("input_tokens", 0)
            tot["cache_read"] += u.get("cached_input_tokens", 0)
            tot["output"] += u.get("output_tokens", 0)
    tot["answer"] = last.read_text() if last.exists() else ""
    return tot


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("tasks", type=Path)
    ap.add_argument("--harness", choices=["claude", "codex", "both"], default="both")
    ap.add_argument("--fox", default=str(Path.home() / ".foxdb/bin/fox"))
    ap.add_argument("--work", type=Path, default=None, help="scratch dir for repo copies (default: a new temp dir)")
    ap.add_argument("--headroom", choices=["on", "off"], default="off")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--max-usd", type=float, default=1.0, help="per-run cap for claude")
    ap.add_argument("--out", type=Path, default=Path("ab-results.jsonl"))
    ap.add_argument("--run", action="store_true", help="actually run (default: dry-run, print commands)")
    a = ap.parse_args()

    spec = tomllib.loads(a.tasks.read_text())
    repo = Path(os.path.expanduser(spec["repo"]))
    harnesses = ["claude", "codex"] if a.harness == "both" else [a.harness]
    work = a.work or Path(tempfile.mkdtemp(prefix="fox-ab-"))
    work.mkdir(parents=True, exist_ok=True)
    env = env_for(a.headroom)
    mode = f"headroom-{a.headroom}"
    print(f"# {len(spec['task'])} tasks x {harnesses} x [on, off] x {a.repeat} · {mode} · work {work}"
          + ("" if a.run else " · DRY RUN"))

    out = a.out.open("a") if a.run else None
    for rep in range(a.repeat):
        for t in spec["task"]:
            for h in harnesses:
                for arm in ("on", "off"):
                    cwd = work / f"{t['id']}-{h}-{arm}-{rep}"
                    clone = ["git", "clone", "--quiet", "--shared", str(repo), str(cwd)]
                    last = work / f"{t['id']}-{h}-{arm}-{rep}.last.txt"
                    if h == "claude":
                        cmd = claude_cmd(t["prompt"], work / f"mcp-{arm}.json", a.max_usd)
                    else:
                        cmd = codex_cmd(t["prompt"], cwd, a.fox, arm == "on", last)
                    if not a.run:
                        print(shlex.join(clone))
                        print(f"(cd {shlex.quote(str(cwd))} && {shlex.join(cmd)})")
                        continue
                    mcp_config(work, a.fox, arm == "on")
                    if not cwd.exists():
                        subprocess.run(clone, check=True)
                    t0 = time.monotonic()
                    p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=1800)
                    secs = time.monotonic() - t0
                    u = usage_from_claude(p.stdout) if h == "claude" else usage_from_codex(p.stdout, last)
                    ans = u.pop("answer", "") or ""
                    row = {"task": t["id"], "harness": h, "foxdb": arm, "rep": rep, "headroom": mode,
                           "exit": p.returncode, "secs": round(secs, 1), **u,
                           "pass": (t["check"] in ans) if t.get("check") else None}
                    out.write(json.dumps(row) + "\n")
                    out.flush()
                    print(json.dumps(row))
    return 0


if __name__ == "__main__":
    sys.exit(main())
