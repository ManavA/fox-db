#!/usr/bin/env python3
"""Does the `fox crew` environment change prompt caching? Run one scripted three-turn task through
`claude -p --output-format stream-json` twice: once plain, once with the crew variables
(FOXDB_AGENT, FOXDB_PARENT). Turn one starts the session with --session-id; turns two and three
continue it with --resume. Each arm prints its cache-read share of input tokens:

    cache_read / (cache_read + cache_creation + uncached input)

summed over the three result events (modelUsage when present, else usage).

Both arms get their own scratch FOXDB_HOME with bin/fox linked to --fox and the scratch project
indexed. A configured foxdb hook can use "$FOXDB_HOME/bin/fox" in either arm. Claude Code's
session markers of the calling session (CLAUDECODE, CLAUDE_CODE_SESSION_ID, ...) are dropped so each run is a top-level session,
and ANTHROPIC_BASE_URL is dropped unless --keep-base-url. The run stops once the reported cost
reaches --max-usd.

    python3 bench/crew_cache.py --model claude-haiku-5-5 --max-usd 0.50
"""

import argparse
import json
import math
import os
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

TURNS = [
    "Remember the number 41. Reply with the single word: ok",
    "Add 1 to the number you remembered. Reply with the result only.",
    "Multiply that result by 2. Reply with the result only.",
]

# The caller's Claude Code session markers, as src/crew.rs SESSION_MARKERS drops them.
SESSION_MARKERS = {
    "CLAUDECODE", "CLAUDE_PID", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_CHILD_NOTES_DIR", "CLAUDE_CODE_EXECPATH", "CLAUDE_CODE_SSE_PORT", "CODEX_THREAD_ID",
}


def usage(stream: str) -> dict:
    """The last result event's usage: modelUsage summed over models, else usage."""
    res = None
    for line in stream.splitlines():
        try:
            j = json.loads(line)
        except json.JSONDecodeError:
            continue
        if j.get("type") == "result":
            res = j
    if res is None:
        return {"error": "no result event"}
    models = res.get("modelUsage") or {}
    u = res.get("usage") or {}
    if models:
        read = sum(m.get("cacheReadInputTokens", 0) for m in models.values())
        made = sum(m.get("cacheCreationInputTokens", 0) for m in models.values())
        fresh = sum(m.get("inputTokens", 0) for m in models.values())
    else:
        read, made, fresh = (u.get(k, 0) for k in ("cache_read_input_tokens", "cache_creation_input_tokens", "input_tokens"))
    return {"read": read, "made": made, "fresh": fresh, "cost": res.get("total_cost_usd") or 0.0,
            "answer": (res.get("result") or "").strip(), "error": res.get("is_error") and res.get("subtype")}


def ratio(read: int, made: int, fresh: int) -> float:
    total = read + made + fresh
    return read / total if total else 0.0


def scratch_home(root: Path, fox: Path, project: Path) -> Path:
    home = root / "fx"
    (home / "bin").mkdir(parents=True)
    (home / "bin" / "fox").symlink_to(fox)
    env = dict(os.environ, FOXDB_HOME=str(home), FOXDB_NO_DAEMON="1")
    subprocess.run([str(fox), "index", str(project), "--quiet", "--no-global"], env=env, check=True, timeout=120)
    return home


def base_env(keep_base_url: bool) -> dict:
    env = {k: v for k, v in os.environ.items() if not k.startswith("FOXDB_") and k not in SESSION_MARKERS}
    if not keep_base_url:
        env.pop("ANTHROPIC_BASE_URL", None)
    return env


def run_arm(name: str, env: dict, project: Path, model: str, per_call: float, spent: list, cap: float) -> dict:
    sid = str(uuid.uuid4())
    tot = {"read": 0, "made": 0, "fresh": 0, "cost": 0.0}
    for i, prompt in enumerate(TURNS):
        budget = math.floor(min(per_call, cap - spent[0]) * 1000) / 1000
        if budget <= 0:
            sys.exit(f"stopping: ${spent[0]:.3f} spent, cap ${cap:.2f}")
        cmd = ["claude", "-p", prompt, "--output-format", "stream-json", "--verbose", "--model", model,
               "--max-budget-usd", f"{budget:.3f}"]
        cmd += ["--session-id", sid] if i == 0 else ["--resume", sid]
        p = subprocess.run(cmd, cwd=project, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=900)
        u = usage(p.stdout)
        if p.returncode or "read" not in u or u.get("error"):
            sys.exit(f"{name} turn {i + 1} failed (exit {p.returncode}): {u} {p.stderr[-400:]}")
        spent[0] += u["cost"]
        if spent[0] > cap:
            sys.exit(f"stopping: Claude reported ${spent[0]:.3f}, above the ${cap:.2f} cap")
        for k in tot:
            tot[k] += u[k]
        print(f"  {name} turn {i + 1}: read {u['read']} created {u['made']} uncached {u['fresh']}"
              f"  ratio {ratio(u['read'], u['made'], u['fresh']):.3f}  answer {u['answer'][:30]!r}")
    return tot


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--model", default="claude-haiku-5-5")
    ap.add_argument("--max-usd", type=float, default=0.50, help="stop once the reported cost reaches this")
    ap.add_argument("--fox", default=str(Path(__file__).resolve().parent.parent / "target/release/fox"))
    ap.add_argument("--work", help="scratch directory (default: a new temp dir)")
    ap.add_argument("--keep-base-url", action="store_true", help="keep ANTHROPIC_BASE_URL (a local proxy)")
    a = ap.parse_args()
    if not math.isfinite(a.max_usd) or a.max_usd < 0.006:
        ap.error("--max-usd must be finite and allow at least $0.001 per turn")

    fox = Path(a.fox).resolve()
    if not fox.is_file():
        sys.exit(f"no fox binary at {fox}: cargo build --release, or pass --fox")
    work = Path(a.work or tempfile.mkdtemp(prefix="crew-cache-"))
    live = Path.home() / ".foxdb"
    if work.resolve().is_relative_to(live.resolve()):
        sys.exit(f"refusing a scratch dir inside the live store {live}")
    project = work / "project"
    project.mkdir(parents=True)
    (project / "README.md").write_text("# scratch\nA scratch project for bench/crew_cache.py.\n")
    subprocess.run(["git", "init", "-q"], cwd=project, check=True)

    env = base_env(a.keep_base_url)
    plain = dict(env, FOXDB_HOME=str(scratch_home(work / "plain", fox, project)))
    crew = dict(env, FOXDB_HOME=str(scratch_home(work / "crew", fox, project)),
                FOXDB_AGENT="cl-crewbench", FOXDB_PARENT="crew:bench")

    per_call = a.max_usd / (2 * len(TURNS))
    spent = [0.0]
    print(f"model {a.model}, cap ${a.max_usd:.2f} (up to ${math.floor(per_call * 1000) / 1000:.3f} per call), work {work}")
    results = {}
    for name, e in (("plain", plain), ("crew", crew)):
        t = run_arm(name, e, project, a.model, per_call, spent, a.max_usd)
        results[name] = ratio(t["read"], t["made"], t["fresh"])
        print(f"{name}: cache-read ratio {results[name]:.3f} over {len(TURNS)} turns, ${t['cost']:.4f}")
    print(f"total ${spent[0]:.4f}")


if __name__ == "__main__":
    main()
