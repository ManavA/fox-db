#!/usr/bin/env python3
"""Scripted agent A/B runner: the same task list with foxdb on and off, through `claude -p` and
`codex exec`.

Dry-run by default: it prints every command it would run and runs nothing. Pass --run to execute;
that spends real model usage, so the controller decides when.

Each task runs in a throwaway copy of the repo (`git clone --shared` into --work), never in the
original. foxdb "on" means the `fox mcp` server is the only MCP server; "off" means no MCP servers. User
settings are skipped for claude (`--setting-sources project,local`) so a user-level foxdb hook
cannot leak into the "off" arm.

Store: with --store scratch (the default) each "on" run gets its own FOXDB_HOME under --work that
holds only that clone's index (`fox index CLONE --no-global`), and FOXDB_CLAUDE_PROJECTS points at
an empty directory, so no memory notes or past sessions can leak an answer. --store live uses
whatever store the environment names (normally ~/.foxdb).

Order: each repeat visits the tasks in a shuffled order and runs the two arms of a task back to
back in a shuffled order. --seed fixes both; every row records it.

Headroom is per process only: --headroom on sets ANTHROPIC_BASE_URL to the local proxy for the
child processes, off removes it. Results go to --out as JSON lines, one per (task, harness, arm,
rep), carrying headroom and store mode, seed, model, harness version, fox sha256 and the git
commits of the target repo and of this harness.

Task file (TOML):
    repo = "~/foxdb"
    [[task]]
    id = "callers-open-ro"
    prompt = "List every function that calls open_ro and say what each uses it for."
    check = "open_rw"        # optional substring the final answer must contain
    expect = ["cmd_find", {name = "db::open_ro", kind = "call"}, {name = "src/tiers.rs", kind = "file"}]
    universe = ["db::open_rw"]  # optional extra repo names; the scratch index supplies the rest
    pass_f1 = 0.8            # optional, default 0.8

Grading with `expect` (kinds: call, store, doc, file; "any" when unlabelled): the universe is every
symbol (as `<file stem>::<name>`) and file in a scratch index of the repo that the agents never
see, plus the task's `universe` and `expect`. The answer's words are matched against it, with
or without backticks, `:line` and `()` dropped. A word names an item when it equals it, when the
item is qualified and the word is its last segment or ends in `::item`, or when the word is
qualified, its last segment is the (unqualified) item and no other universe name shares that
segment. Each item counts once. A false positive is a word naming some other universe symbol
(a plain word such as `run` or `Config` only when backticked or written `run()`, since prose uses
them too);
paths count as false positives only in tasks with `file` items; words outside the universe are
ignored. A task passes when F1 reaches pass_f1 and its `check`, if any, holds.
--grader-selftest grades two prose oracles per task (the items as written, then with qualified and
unqualified forms swapped on alternate items; both must score 1), an empty answer (must score 0)
and an oracle padded with 50 decoys (precision must fall below 0.5), and runs nothing else.

    ab.py report RESULTS.jsonl     paired on-off differences per task and overall, bootstrap CIs,
                                   pass^k and p90 cost, one block per run (seed, model, harness version)
    ab.py report RESULTS.jsonl --tasks TASKS.toml
                                   also per hop depth (F1 and pass differences) and, for tasks whose items
                                   carry fox's label, gold found and wrong answers named per label and the
                                   wrong-certain count (bench/oracle/README.md); --tasks supplies depth and
                                   labels, rows supply the answer

--harness local runs bench/local_agent.py against an OpenAI-compatible server (--local-url, e.g.
llama.cpp llama-server) with three arms instead of on/off: grep (bash only), fox-cli (bash plus the
`fox` CLI on the scratch store) and packet (bash plus a `fox packet` answer context up front;
--local-packet-builder python uses local_agent.py's older builder). The
arm goes in the row's `foxdb` field; report compares fox-cli and packet with grep. Rows add steps,
cap, forced_final, loop and max_repeat; transcripts go to --work as NAME.transcript.jsonl.

Methods and their sources (docs/benchmarks.md#references; Stanford CS329Z, Engineering AI Agents,
Lectures 1, 4, 13, 15 and 16, https://cs329z.stanford.edu/):
    pass^k, estimated as C(c, k) / C(n, k)   Yao et al. 2024, tau-bench, https://arxiv.org/abs/2406.12045
    grader self-test (oracles, empty, padded) and answers kept out of agent clones
                                             Zhu et al. 2025, https://arxiv.org/abs/2507.02825
    per-run cost cap, cost and p90 cost      Kapoor et al. 2024, https://arxiv.org/abs/2407.01502
    real-repository tasks                    Jimenez et al. 2023, SWE-bench, https://arxiv.org/abs/2310.06770
    percentile bootstrap CIs                 Efron 1979, https://doi.org/10.1214/aos/1176344552
    a fresh clone for every run and the grader
                                             Anthropic 2026, https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents
"""

import argparse
import hashlib
import json
import math
import os
import random
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import tomllib
from collections import defaultdict
from pathlib import Path

import local_agent

HEADROOM_URL = "http://127.0.0.1:8787"
LIVE_HOME = Path.home() / ".foxdb"
SOURCE_EXTS = {"rs", "py", "md", "toml", "json", "ts", "tsx", "js", "go", "c", "h", "cc", "cpp", "hpp", "kt",
               "swift", "java", "rb", "sh", "yml", "yaml", "txt", "sql", "proto"}
ARMS = {"claude": ["on", "off"], "codex": ["on", "off"], "local": local_agent.ARMS}
INDEXED_ARMS = {"on", "fox-cli", "packet", "general", "lean", "lean-packet"}
SUBAGENT_TYPES = {"general": "general-purpose", "lean": "lean", "lean-packet": "lean"}
LEAN_AGENT = """---
name: lean
description: Lean worker with shell, file reading and editing only.
tools: Bash, Read, Edit, Write
---
You are a focused worker. Do the task in the prompt and return a concise factual answer.
"""
BASELINES = ("off", "grep")
DECOY_WORDS = ["inner", "impl", "v2", "old", "cached", "raw", "unchecked", "try", "with", "legacy", "async",
               "batch", "ext", "util", "helper", "core", "local", "tmp", "fast", "safe"]


# ---- grading

def expect_items(task: dict) -> list:
    out = []
    for it in task.get("expect", []):
        if isinstance(it, str):
            out.append((it, "any"))
        else:
            out.append((it["name"], it.get("kind", "any")))
    return out


def _clean(tok: str) -> str:
    tok = tok.strip(".,;:-")
    tok = re.sub(r":\d+(?:[-:]\d+)?$", "", tok)
    return tok.removeprefix("./")


def shape(tok: str) -> str:
    ext = tok.rsplit(".", 1)[-1] if "." in tok else ""
    return "path" if "/" in tok or ext in SOURCE_EXTS else "ident"


def last(name: str) -> str:
    return name.rsplit("/", 1)[-1] if shape(name) == "path" else re.split(r"::|\.", name)[-1]


def qualified(name: str) -> bool:
    return last(name) != name


def named_tokens(answer: str) -> set:
    out = set()
    for raw in re.findall(r"[\w./:\-]+", answer):
        tok = _clean(raw)
        for part in [tok] if shape(tok) == "path" else tok.split("-"):
            part = _clean(part)
            if re.match(r"[A-Za-z_./]", part):
                out.add(part)
    return out


def marked_tokens(answer: str) -> set:
    spans = re.findall(r"`([^`\n]+)`", answer) + re.findall(r"([\w.:]+)\(\)", answer)
    return set().union(*(named_tokens(sp) for sp in spans)) if spans else set()


def store_universe(db: Path) -> set:
    c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        rows = c.execute("select kind, name, path from node where kind in ('symbol', 'file')").fetchall()
    finally:
        c.close()
    return {f"{Path(p).stem}::{n}" if k == "symbol" else p for k, n, p in rows if p}


def hits(tok: str, item: str, universe: set) -> bool:
    if tok == item:
        return True
    if qualified(item):
        return tok == last(item) or any(tok.endswith(sep + item) for sep in ("::", ".", "/"))
    return (qualified(tok) and last(tok) == item
            and sum(1 for u in universe if u != item and last(u) == item) <= 1)


def _prf(found: set, items: list, fps: set) -> dict:
    tp = len(found & set(items))
    p = tp / (tp + len(fps)) if tp + len(fps) else 0.0
    r = tp / len(items) if items else 0.0
    f1 = 2 * p * r / (p + r) if p + r else 0.0
    return {"precision": p, "recall": r, "f1": f1}


def grade(answer: str, task: dict, universe: set = frozenset()) -> dict:
    check = task.get("check")
    items = expect_items(task)
    if not items:
        return {"pass": (check in answer) if check else None}
    names = [n for n, _ in items]
    uni = set(universe) | set(task.get("universe", [])) | set(names)
    files = any(k == "file" for _, k in items)
    ident_lasts = {last(u) for u in uni if shape(u) == "ident"}
    paths = [u for u in uni if shape(u) == "path"]
    taken = {last(n) for n in names}
    marked = marked_tokens(answer)
    found, fps = set(), set()
    for tok in named_tokens(answer):
        hit = {n for n in names if hits(tok, n, uni)}
        found |= hit
        if hit or last(tok) in taken:
            continue
        if shape(tok) == "path":
            if files and (tok in uni or any(u.endswith("/" + tok) for u in paths)):
                fps.add(("path", last(tok)))
        elif (tok in uni or (not qualified(tok) and tok in ident_lasts)) and (
                tok in marked or not re.fullmatch(r"[A-Za-z][a-z0-9]*", tok)):
            fps.add(("ident", last(tok)))
    g = _prf(found, names, fps)
    g["by_kind"] = {}
    for kind in sorted({k for _, k in items}):
        mine = [n for n, k in items if k == kind]
        shapes = {shape(n) for n in mine}
        g["by_kind"][kind] = _prf(found, mine, {f for f in fps if f[0] in shapes})
    g["pass"] = g["f1"] >= task.get("pass_f1", 0.8) and (check in answer if check else True)
    return g


def _prose(names: list) -> str:
    joined = names[0] if len(names) == 1 else ", ".join(names[:-1]) + " and " + names[-1]
    return f"The items are {joined}."


def oracle_answer(items: list) -> str:
    return _prose([n for n, _ in items])


def _flip(name: str, universe: set) -> str:
    if qualified(name):
        return last(name)
    owners = [u for u in universe if u != name and last(u) == name]
    if len(owners) == 1:
        return owners[0]
    return ("src/" if shape(name) == "path" else "crate::") + name


def oracle_answers(items: list, universe) -> list:
    names = [n for n, _ in items]
    uni = set(universe) | set(names)
    return [_prose(names), _prose([_flip(n, uni) if i % 2 == 0 else n for i, n in enumerate(names)])]


def _mutate(base: str, word: str, before: bool) -> str:
    cut = max(base.rfind("::") + 2 if "::" in base else 0, base.rfind("/") + 1)
    head, tail = base[:cut], base[cut:]
    stem, dot, ext = tail.rpartition(".") if shape(base) == "path" and "." in tail else (tail, "", "")
    return head + (f"{word}_{stem}" if before else f"{stem}_{word}") + dot + ext


def decoys(items: list, n: int = 50, seed: int = 0, universe=frozenset()) -> list:
    rng = random.Random(seed)
    names = [x for x, _ in items]
    files = any(k == "file" for _, k in items)
    bases = [x for x in names if files or shape(x) == "ident"] or ["helper"]
    uni = set(universe) | set(names)
    taken = {last(x) for x in names}
    out = []
    for _ in range(n * 100):
        if len(out) == n:
            break
        d = _mutate(rng.choice(bases), rng.choice(DECOY_WORDS) + rng.choice(["", "2", "_all", "_of"]),
                    rng.random() < 0.5)
        if d not in out and last(d) not in taken and not any(hits(d, x, uni) for x in names):
            out.append(d)
    return out


def grader_selftest(spec: dict, universe=frozenset()) -> tuple:
    ok, lines = True, []
    for t in spec["task"]:
        items = expect_items(t)
        if not items:
            lines.append(f"{t['id']}: skip (no expect)")
            continue
        uni = set(universe) | set(t.get("universe", []))
        bad = []
        for label, ans in zip(("prose", "mixed"), oracle_answers(items, uni)):
            g = grade(ans, t, uni)
            if g["f1"] != 1.0 or any(k["f1"] != 1.0 for k in g["by_kind"].values()):
                bad.append(f"{label} oracle f1 {g['f1']:.3f}")
        empty = grade("", t, uni)
        if empty["f1"] != 0.0:
            bad.append(f"empty f1 {empty['f1']:.3f}")
        ds = decoys(items, universe=uni)
        dump = grade(oracle_answer(items) + "\n" + "\n".join(ds), t, uni | set(ds))
        if dump["precision"] >= 0.5:
            bad.append(f"dump precision {dump['precision']:.3f}")
        ok = ok and not bad
        lines.append(f"{t['id']}: " + ("FAIL " + "; ".join(bad) if bad else
                                       f"ok (oracles 1, empty 0, dump precision {dump['precision']:.3f})"))
    return ok, lines


# ---- order and store

def schedule(task_ids: list, harnesses: list, repeat: int, seed: int) -> list:
    rng = random.Random(seed)
    runs = []
    for rep in range(repeat):
        for tid in rng.sample(task_ids, len(task_ids)):
            for h in harnesses:
                arms = list(ARMS[h])
                rng.shuffle(arms)
                runs += [{"rep": rep, "task": tid, "harness": h, "arm": arm} for arm in arms]
    return runs


def store_plan(store: str, fox: str, clone: Path, home: Path) -> dict:
    if store == "live":
        return {"index": None, "env": {}}
    if home.resolve().is_relative_to(LIVE_HOME.resolve()):
        sys.exit(f"refusing a scratch store inside the live store {LIVE_HOME}: pick another --work")
    return {"index": [fox, "index", str(clone), "--quiet", "--no-global"],
            "env": {"FOXDB_HOME": str(home), "FOXDB_CLAUDE_PROJECTS": str(home / "claude-projects")}}


def fresh_home(home: Path) -> None:
    if home.resolve().is_relative_to(LIVE_HOME.resolve()):
        sys.exit(f"refusing to wipe {home}: it is inside the live store {LIVE_HOME}")
    shutil.rmtree(home, ignore_errors=True)
    (home / "claude-projects").mkdir(parents=True)


def fresh_clone(clone: list, dst: Path, repo: Path, hidden: list) -> None:
    if dst.resolve().is_relative_to(LIVE_HOME.resolve()):
        sys.exit(f"refusing to wipe {dst}: it is inside the live store {LIVE_HOME}")
    shutil.rmtree(dst, ignore_errors=True)
    subprocess.run(clone, check=True)
    for p in hidden:
        if p.resolve().is_relative_to(repo.resolve()):
            leak = dst / p.resolve().relative_to(repo.resolve())
            if leak.is_dir():
                shutil.rmtree(leak)
            else:
                leak.unlink(missing_ok=True)


def env_for(headroom: str, store_env: dict) -> dict:
    env = dict(os.environ)
    if headroom == "on":
        env["ANTHROPIC_BASE_URL"] = HEADROOM_URL
    else:
        env.pop("ANTHROPIC_BASE_URL", None)
    return env | store_env


def mcp_config_json(fox: str, on: bool, store_env: dict) -> dict:
    if not on:
        return {"mcpServers": {}}
    server = {"command": fox, "args": ["mcp"]}
    if store_env:
        server["env"] = store_env
    return {"mcpServers": {"foxdb": server}}


def delegate_prompt(prompt: str, arm: str, packet: str) -> str:
    brief = prompt + (f"\n\nContext from the code index:\n{packet}" if packet else "")
    return (f"Delegate this whole task to one subagent with the Agent tool, subagent_type {SUBAGENT_TYPES[arm]},"
            f" passing it this brief verbatim, then return its answer.\n\nBrief:\n{brief}")


def claude_cmd(prompt: str, cfg: Path, max_usd: float, model) -> list:
    cmd = ["claude", "-p", prompt, "--output-format", "json", "--strict-mcp-config", "--mcp-config", str(cfg),
           "--no-session-persistence", "--max-budget-usd", str(max_usd),
           # user-level hooks (a foxdb SessionStart brief) would leak into the "off" arm
           "--setting-sources", "project,local"]
    return cmd + (["--model", model] if model else [])


def codex_cmd(prompt: str, cwd: Path, fox: str, on: bool, last: Path, store_env: dict, model) -> list:
    cmd = ["codex", "exec", "--json", "--ephemeral", "--skip-git-repo-check", "-s", "read-only", "-C", str(cwd),
           "-o", str(last)]
    if on:
        env = ",".join(f"{k}={json.dumps(v)}" for k, v in store_env.items())
        server = f"command={json.dumps(fox)},args=[\"mcp\"]" + (f",env={{{env}}}" if env else "")
        cmd += ["-c", f"mcp_servers={{foxdb={{{server}}}}}"]
    else:
        cmd += ["-c", "mcp_servers={}"]  # drop any user-configured foxdb server
    return cmd + (["-m", model] if model else []) + [prompt]


def local_cmd(prompt: str, arm: str, cwd: Path, fox: str, scratch: Path, transcript: Path, a) -> list:
    cmd = [sys.executable, str(Path(__file__).resolve().parent / "local_agent.py"), "--prompt", prompt, "--arm", arm,
           "--cwd", str(cwd), "--scratch", str(scratch), "--fox", fox, "--url", a.local_url,
           "--steps", str(a.local_steps), "--token-cap", str(a.local_token_cap),
           "--reasoning-effort", a.local_effort, "--packet-builder", a.local_packet_builder,
           "--transcript", str(transcript)]
    return cmd + (["--model", a.model] if a.model else [])


# ---- provenance and usage

def sha256_file(path) -> str | None:
    try:
        return hashlib.sha256(Path(path).read_bytes()).hexdigest()
    except OSError:
        return None


def _out(cmd: list) -> str | None:
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return p.stdout.strip() if p.returncode == 0 else None


def usage_from_claude(stdout: str) -> dict:
    try:
        j = json.loads(stdout)
    except json.JSONDecodeError:
        return {"parse_error": True}
    u = j.get("usage", {})
    models = j.get("modelUsage") or {}
    cache_read = sum(m.get("cacheReadInputTokens", 0) for m in models.values()) if models \
        else u.get("cache_read_input_tokens")
    cache_write = sum(m.get("cacheCreationInputTokens", 0) for m in models.values()) if models \
        else u.get("cache_creation_input_tokens")
    return {"cost_usd": j.get("total_cost_usd"), "turns": j.get("num_turns"), "input": u.get("input_tokens"),
            "cache_read": cache_read, "cache_write": cache_write,
            "output": u.get("output_tokens"), "model": ",".join(sorted(j.get("modelUsage", {}))) or None,
            "answer": j.get("result", "")}


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


def usage_from_local(stdout: str) -> dict:
    try:
        j = json.loads(stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        return {"parse_error": True}
    keep = ("steps", "input", "output", "cap", "forced_final", "commands", "format_errors", "loop", "loop_commands",
            "max_repeat", "packet_chars", "packet_builder", "answer")
    server = j.get("server") or {}
    return {"turns": j.get("steps"), "agent_secs": j.get("secs"), "ctx": server.get("ctx"),
            "model": server.get("model"), **{k: j.get(k) for k in keep}}


def local_version(url: str) -> str | None:
    info = local_agent.server_info(url)
    return f"llama.cpp {info['build']} ctx {info['ctx']}" if info else None


# ---- report

def pass_hat_k(n: int, c: int, k: int) -> float:
    return math.comb(c, k) / math.comb(n, k)


def percentile(xs: list, q: float) -> float:
    s = sorted(xs)
    pos = (len(s) - 1) * q / 100
    lo = math.floor(pos)
    hi = min(lo + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (pos - lo)


def bootstrap_ci(xs: list, seed: int = 0, draws: int = 10000) -> tuple:
    rng = random.Random(seed)
    means = sorted(sum(rng.choices(xs, k=len(xs))) / len(xs) for _ in range(draws))
    return means[int(0.025 * draws)], means[math.ceil(0.975 * draws) - 1]


METRICS = [("cost", "cost_usd", "{:+.4f}"), ("turns", "turns", "{:+.2f}"), ("f1", "f1", "{:+.3f}"),
           ("pass", "pass", "{:+.2f}"), ("secs", "secs", "{:+.1f}"), ("loop", "loop", "{:+.2f}")]


def _num(row: dict, key: str):
    v = row.get(key)
    return float(v) if isinstance(v, (int, float)) else None


def _diff_text(name: str, fmt: str, xs: list, seed: int, draws: int) -> str:
    if not xs:
        return ""
    lo, hi = bootstrap_ci(xs, seed, draws)
    return f"{name} {fmt.format(sum(xs) / len(xs))} [{fmt.format(lo)}, {fmt.format(hi)}]"


def _pass_text(judged: list, x: str, b: str) -> str:
    if not judged:
        return ""
    k = min(min(len(p[x]), len(p[b])) for p in judged)
    out = []
    for kk in sorted({1, k}):
        hat = {arm: sum(pass_hat_k(len(p[arm]), sum(p[arm]), kk) for p in judged) / len(judged) for arm in (x, b)}
        out.append(f"pass^{kk} {x} {hat[x]:.2f} {b} {hat[b]:.2f}")
    return "  ".join(out)


def _p90_text(by_arm: dict, x: str, b: str) -> str:
    costs = {arm: [c for r in rows if (c := _num(r, "cost_usd")) is not None] for arm, rows in by_arm.items()}
    if not (costs[x] and costs[b]):
        return ""
    return f"p90 cost {x} {percentile(costs[x], 90):.4f} {b} {percentile(costs[b], 90):.4f}"


def comparisons(arms: set) -> list:
    base = next((b for b in BASELINES if b in arms), min(arms))
    return [(x, base) for x in sorted(arms) if x != base]


LABELS = ("certain", "guess", "missing")


def fox_rows(answer: str, task: dict) -> dict | None:
    """Per fox label, [named, offered] for the gold items (`expect` rows with a `fox` label) and for
    `fox_extra` (what fox answers that the oracle says is wrong). An extra whose name a gold item
    shares cannot be told apart by name and is left out. None when the task has no fox labels."""
    items = [it for it in task.get("expect", []) if isinstance(it, dict) and "fox" in it]
    if not items:
        return None
    gold = {it["name"] for it in items}
    extra = [e for e in task.get("fox_extra", []) if e["name"] not in gold]
    uni = gold | {e["name"] for e in extra} | set(task.get("universe", []))
    toks = named_tokens(answer)
    named = {n for n in uni if any(hits(t, n, uni) for t in toks)}
    out = {"gold": {}, "wrong": {}}
    for part, rows in (("gold", items), ("wrong", extra)):
        for r in rows:
            c = out[part].setdefault(r["fox"], [0, 0])
            c[0] += r["name"] in named
            c[1] += 1
    return out


def _depth(row: dict, tasks: dict):
    d = row.get("depth")
    return d if d is not None else tasks.get(row["task"], {}).get("depth")


def _counts(counts: dict, labels) -> str:
    return " ".join(f"{k} {counts[k][0]}/{counts[k][1]}" for k in labels if k in counts) or "none offered"


def _label_lines(depth, by_task: dict, tasks: dict, arms: tuple) -> list:
    """Gold found and wrong answers named, per fox label, summed over each arm's rows."""
    tot = {arm: {"gold": {}, "wrong": {}} for arm in arms}
    seen = {arm: 0 for arm in arms}
    labelled = False
    for tid, by_arm in by_task.items():
        if tid not in tasks:
            continue
        for arm in arms:
            for r in by_arm.get(arm, {}).values():
                if "answer" not in r:
                    labelled = labelled or fox_rows("", tasks[tid]) is not None
                    continue
                got = fox_rows(r["answer"] or "", tasks[tid])
                if got is None:
                    continue
                seen[arm] += 1
                for part in ("gold", "wrong"):
                    for k, (n, m) in got[part].items():
                        c = tot[arm][part].setdefault(k, [0, 0])
                        c[0] += n
                        c[1] += m
    tag = f"depth {depth}"
    if not any(seen.values()):
        return [f"  {tag} fox labels: no answers recorded"] if labelled else []
    on = [arm for arm in arms if seen[arm]]
    return [f"  {tag} gold found, by fox label · " + " · ".join(f"{arm} {_counts(tot[arm]['gold'], LABELS)}" for arm in on),
            f"  {tag} wrong answers fox also gave, named/offered by fox label · "
            + " · ".join(f"{arm} {_counts(tot[arm]['wrong'], LABELS[:2])}" for arm in on),
            f"  {tag} wrong-certain " + " ".join(f"{arm} {tot[arm]['wrong'].get('certain', [0, 0])[0]}" for arm in on)
            + f" (answers over {', '.join(f'{seen[arm]} {arm}' for arm in on)} rows)"]


def report(rows: list, seed: int = 0, draws: int = 10000, tasks: dict | None = None) -> str:
    tasks = tasks or {}
    groups = defaultdict(lambda: defaultdict(lambda: defaultdict(dict)))
    depths = {}
    for r in rows:
        key = (r["harness"], r.get("headroom", "unknown"), r.get("store", "unknown"), str(r.get("seed", "unknown")),
               r.get("model_requested", r.get("model")) or "unknown", r.get("harness_version") or "unknown")
        groups[key][r["task"]][r["foxdb"]][r.get("rep", 0)] = r
        if _depth(r, tasks) is not None:
            depths[r["task"]] = _depth(r, tasks)
    lines = ["differences are on minus off (local harness: each arm minus grep); brackets are bootstrap 95% CIs"
             " (per task over reps, overall and per depth over tasks); depth = hops of direct calls from the target"]
    for (h, hr, store, seed_, model, ver), by_task in sorted(groups.items()):
        for x, b in comparisons({arm for t in by_task.values() for arm in t}):
            label = "" if (x, b) == ("on", "off") else f" · {x} minus {b}"
            lines.append(f"\n{h} · {hr} · store {store} · seed {seed_} · model {model} · {ver}{label}")
            task_means = defaultdict(dict)
            judged = {}
            for tid, arms in sorted(by_task.items()):
                reps = sorted(set(arms[x]) & set(arms[b]))
                parts = [f"{tid:<24} n {len(reps)}"]
                for name, key, fmt in METRICS:
                    xs = [_num(arms[x][i], key) - _num(arms[b][i], key) for i in reps
                          if _num(arms[x][i], key) is not None and _num(arms[b][i], key) is not None]
                    if xs:
                        task_means[name][tid] = sum(xs) / len(xs)
                    parts.append(_diff_text(name, fmt, xs, seed, draws))
                passes = {arm: [r["pass"] for r in arms[arm].values() if isinstance(r.get("pass"), bool)]
                          for arm in (x, b)}
                if passes[x] and passes[b]:
                    judged[tid] = passes
                    parts.append(_pass_text([passes], x, b))
                parts.append(_p90_text({arm: list(arms[arm].values()) for arm in (x, b)}, x, b))
                lines.append("  ".join(p for p in parts if p))
            parts = [f"{'ALL':<24} tasks {len(by_task)}"]
            parts += [_diff_text(name, fmt, list(task_means[name].values()), seed, draws) for name, _, fmt in METRICS]
            parts.append(_pass_text(list(judged.values()), x, b))
            parts.append(_p90_text({arm: [r for t in by_task.values() for r in t[arm].values()] for arm in (x, b)}, x, b))
            lines.append("  ".join(p for p in parts if p))
            for d in sorted({depths[t] for t in by_task if t in depths}):
                mine = sorted(t for t in by_task if depths.get(t) == d)
                parts = [f"depth {d:<18} tasks {len(mine)}"]
                parts += [_diff_text(name, fmt, [task_means[name][t] for t in mine if t in task_means[name]], seed,
                                     draws) for name, _, fmt in METRICS if name in ("f1", "pass")]
                parts.append(_pass_text([judged[t] for t in mine if t in judged], x, b))
                lines.append("  ".join(p for p in parts if p))
                lines += _label_lines(d, {t: by_task[t] for t in mine}, tasks, (x, b))
    return "\n".join(lines)


# ---- CLI

def run_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("tasks", type=Path)
    ap.add_argument("--harness", choices=["claude", "codex", "both", "local"], default="both",
                    help="both = claude and codex; local = bench/local_agent.py on --local-url")
    ap.add_argument("--fox", default=str(LIVE_HOME / "bin/fox"))
    ap.add_argument("--work", type=Path, default=None, help="scratch dir for repo copies (default: a new temp dir)")
    ap.add_argument("--headroom", choices=["on", "off"], default="off")
    ap.add_argument("--store", choices=["scratch", "live"], default="scratch")
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--seed", type=int, default=None, help="order seed (default: random, recorded)")
    ap.add_argument("--model", default=None, help="model passed to both harnesses (default: theirs)")
    ap.add_argument("--max-usd", type=float, default=1.0, help="per-run cap for claude")
    ap.add_argument("--local-url", default="http://127.0.0.1:18431", help="OpenAI-compatible server for local")
    ap.add_argument("--local-steps", type=int, default=20, help="turn cap for local (one last answer turn after)")
    ap.add_argument("--local-token-cap", type=int, default=200_000, help="prompt plus completion tokens, all turns")
    ap.add_argument("--local-effort", default="low", help="reasoning_effort chat template kwarg; '' for default")
    ap.add_argument("--subagent", choices=sorted(SUBAGENT_TYPES), default=None,
                    help="claude: one arm, fox MCP on, task delegated to this subagent setup")
    ap.add_argument("--local-packet-builder", choices=["fox", "python"], default="fox",
                    help="packet arm: `fox packet`, or local_agent.py's older Python builder")
    ap.add_argument("--out", type=Path, default=Path("ab-results.jsonl"))
    ap.add_argument("--grader-selftest", action="store_true", help="check the grader on each task and exit")
    ap.add_argument("--run", action="store_true", help="actually run (default: dry-run, print commands)")
    ap.add_argument("--resume", action="store_true", help="skip (task, harness, arm, rep) cells already in --out")
    return ap


def report_main(argv: list) -> int:
    ap = argparse.ArgumentParser(prog="ab.py report")
    ap.add_argument("results", type=Path)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--draws", type=int, default=10000)
    ap.add_argument("--tasks", type=Path, action="append", default=[],
                    help="task file(s) giving depth and fox labels, for rows that do not carry them")
    a = ap.parse_args(argv)
    rows = [json.loads(ln) for ln in a.results.read_text().splitlines() if ln.strip()]
    tasks = {t["id"]: t for f in a.tasks for t in tomllib.loads(f.read_text()).get("task", [])}
    print(report(rows, a.seed, a.draws, tasks))
    return 0


def done_cells(out: Path) -> set:
    cells = set()
    if out.exists():
        for line in out.read_text().splitlines():
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue
            cells.add((r["task"], r["harness"], r["foxdb"], r["rep"]))
    return cells


def main(argv: list | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["report"]:
        return report_main(argv[1:])
    a = run_parser().parse_args(argv)
    spec = tomllib.loads(a.tasks.read_text())
    if a.grader_selftest:
        ok, lines = grader_selftest(spec)
        print("\n".join(lines))
        return 0 if ok else 1

    repo = Path(os.path.expanduser(spec["repo"]))
    tasks = {t["id"]: t for t in spec["task"]}
    harnesses = ["claude", "codex"] if a.harness == "both" else [a.harness]
    if a.harness == "local" and a.model is None:
        a.model = (local_agent.server_info(a.local_url).get("model") if a.run else None)
    seed = a.seed if a.seed is not None else random.SystemRandom().randrange(2**31)
    work = (a.work or Path(tempfile.mkdtemp(prefix="fox-ab-"))).resolve()
    work.mkdir(parents=True, exist_ok=True)
    mode = f"headroom-{a.headroom}"
    fox_sha = sha256_file(a.fox)
    if a.subagent:
        ARMS["claude"] = [a.subagent]
    arms = sorted({arm for h in harnesses for arm in ARMS[h]})
    print(f"# {len(tasks)} tasks x {harnesses} x {arms} x {a.repeat} · {mode} · store {a.store} · seed {seed}"
          f" · fox {fox_sha or 'missing'} · work {work}" + ("" if a.run else " · DRY RUN"))

    prov = {"seed": seed, "store": a.store, "fox_sha256": fox_sha, "model_requested": a.model or "default"}
    gclone, ghome = work / "grader-clone", work / "grader.home"
    gplan = store_plan("scratch", a.fox, gclone, ghome)
    gsteps = [["git", "clone", "--quiet", "--shared", str(repo), str(gclone)], gplan["index"]]
    versions, universe = {}, set()
    if not a.run:
        print("# grader universe: a scratch index of the repo the agents never see")
        print(shlex.join(gsteps[0]))
        print(" ".join(f"{k}={shlex.quote(v)}" for k, v in gplan["env"].items()), shlex.join(gsteps[1]))
    else:
        fresh_clone(gsteps[0], gclone, repo, [])
        fresh_home(ghome)
        subprocess.run(gsteps[1], env=env_for(a.headroom, gplan["env"]), check=True)
        universe = store_universe(ghome / "fox.db")
        prov["repo_commit"] = _out(["git", "-C", str(repo), "rev-parse", "HEAD"])
        prov["bench_commit"] = _out(["git", "-C", str(Path(__file__).resolve().parent), "rev-parse", "HEAD"])
        versions = {h: local_version(a.local_url) if h == "local" else _out([h, "--version"]) for h in harnesses}
    out = a.out.open("a") if a.run else None
    done = done_cells(a.out) if a.resume else set()
    for slot, s in enumerate(schedule(list(tasks), harnesses, a.repeat, seed)):
        t, h, arm, rep = tasks[s["task"]], s["harness"], s["arm"], s["rep"]
        if (t["id"], h, arm, rep) in done:
            continue
        name = f"{t['id']}-{h}-{arm}-{rep}"
        cwd = work / name
        clone = ["git", "clone", "--quiet", "--shared", str(repo), str(cwd)]
        last = work / f"{name}.last.txt"
        plan = store_plan(a.store, a.fox, cwd, work / f"{name}.home")
        cfg = work / f"{name}.mcp.json"
        prompt = delegate_prompt(t["prompt"], arm, "<packet>" if arm == "lean-packet" else "") \
            if arm in SUBAGENT_TYPES else t["prompt"]
        if h == "claude":
            cmd = claude_cmd(prompt, cfg, a.max_usd, a.model)
        elif h == "codex":
            cmd = codex_cmd(t["prompt"], cwd, a.fox, arm == "on", last, plan["env"], a.model)
        else:
            cmd = local_cmd(t["prompt"], arm, cwd, a.fox, work / f"{name}.agent", work / f"{name}.transcript.jsonl", a)
        index = plan["index"] if arm in INDEXED_ARMS else None
        if not a.run:
            print(f"# slot {slot}: {name}")
            print(shlex.join(clone))
            if index:
                envs = " ".join(f"{k}={shlex.quote(v)}" for k, v in plan["env"].items())
                print(f"{envs} {shlex.join(index)}")
            print(f"(cd {shlex.quote(str(cwd))} && {shlex.join(cmd)})")
            continue
        if h != "local":
            cfg.write_text(json.dumps(mcp_config_json(a.fox, arm == "on" or arm in SUBAGENT_TYPES, plan["env"])))
        fresh_clone(clone, cwd, repo, [a.tasks, repo / "bench" / "tasks", repo / "bench" / "private"])
        env = env_for(a.headroom, plan["env"])
        if index:
            fresh_home(work / f"{name}.home")
            subprocess.run(index, env=env, check=True)
        if arm in SUBAGENT_TYPES:
            (cwd / ".claude" / "agents").mkdir(parents=True, exist_ok=True)
            (cwd / ".claude" / "agents" / "lean.md").write_text(LEAN_AGENT)
            packet = local_agent.build_packet(t["prompt"], a.fox, cwd, env) if arm == "lean-packet" else ""
            cmd[2] = delegate_prompt(t["prompt"], arm, packet)
        t0 = time.monotonic()
        p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True,
                           timeout=7200 if h == "local" else 1800)
        secs = time.monotonic() - t0
        if h == "claude":
            u = usage_from_claude(p.stdout)
        elif h == "codex":
            u = usage_from_codex(p.stdout, last)
        else:
            u = usage_from_local(p.stdout)
        ans = u.pop("answer", "") or ""
        model = u.pop("model", None) or a.model or "default"
        row = {"task": t["id"], "depth": t.get("depth"), "harness": h, "foxdb": arm, "rep": rep, "slot": slot, "headroom": mode,
               "exit": p.returncode, "secs": round(secs, 1), **u, **grade(ans, t, universe), "model": model,
               "harness_version": versions.get(h), "answer": ans, **prov}
        out.write(json.dumps(row) + "\n")
        out.flush()
        print(json.dumps(row))
        for d in (cwd, work / f"{name}.home", work / f"{name}.agent"):
            shutil.rmtree(d, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
