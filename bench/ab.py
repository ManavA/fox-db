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
    expect = ["cmd_find", {name = "db::open_ro", kind = "call"}, {name = "tiers.rs", kind = "doc"}]
    pass_f1 = 0.8            # optional, default 0.8

Grading with `expect`: the grader collects the code-like names in the final answer (backticked
text, identifiers with `_`, `::` or camelCase, paths and file names; `:line` suffixes dropped). A
name matches an item when it equals it or ends in `::item`, `.item` or `/item`. Precision counts
only names shaped like the items (identifier or path), per kind (call, store, doc; "any" when
unlabelled). A task passes when its F1 reaches pass_f1 and its `check`, if any, holds.
--grader-selftest grades each task's oracle answer (must score 1), an empty answer (must score 0)
and the oracle padded with 50 decoys (precision must fall below 0.5), and runs nothing else.

    ab.py report RESULTS.jsonl     paired on-off differences per task and overall, bootstrap CIs,
                                   pass^k and p90 cost
"""

import argparse
import hashlib
import json
import math
import os
import random
import re
import shlex
import subprocess
import sys
import tempfile
import time
import tomllib
from collections import defaultdict
from pathlib import Path

HEADROOM_URL = "http://127.0.0.1:8787"
LIVE_HOME = Path.home() / ".foxdb"
SOURCE_EXTS = {"rs", "py", "md", "toml", "json", "ts", "tsx", "js", "go", "c", "h", "cc", "cpp", "hpp", "kt",
               "swift", "java", "rb", "sh", "yml", "yaml", "txt", "sql", "proto"}
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
    tok = tok.removesuffix("()")
    return tok.removeprefix("./")


def shape(tok: str) -> str:
    ext = tok.rsplit(".", 1)[-1] if "." in tok else ""
    return "path" if "/" in tok or ext in SOURCE_EXTS else "ident"


def _code_like(tok: str) -> bool:
    return ("_" in tok or "::" in tok or "/" in tok or shape(tok) == "path"
            or re.search(r"[a-z][A-Z]", tok) is not None)


def named_tokens(answer: str) -> set:
    names = set()
    for span in re.findall(r"`([^`\n]+)`", answer):
        for raw in re.findall(r"[\w./:\-]+(?:\(\))?", span):
            tok = _clean(raw)
            if re.match(r"[A-Za-z_./]", tok):
                names.add(tok)
    for raw in re.findall(r"[\w./:\-]+", answer):
        tok = _clean(raw)
        if tok and re.match(r"[A-Za-z_./]", tok) and _code_like(tok):
            names.add(tok)
    return names


def matches(tok: str, item: str) -> bool:
    return tok == item or any(tok.endswith(sep + item) for sep in ("::", ".", "/"))


def _prf(tokens: set, items: list, others: list) -> dict:
    shapes = {shape(i) for i in items}
    pool = [t for t in tokens if shape(t) in shapes and not any(matches(t, o) for o in others)]
    tp = sum(1 for t in pool if any(matches(t, i) for i in items))
    found = sum(1 for i in items if any(matches(t, i) for t in tokens))
    p = tp / len(pool) if pool else 0.0
    r = found / len(items) if items else 0.0
    f1 = 2 * p * r / (p + r) if p + r else 0.0
    return {"precision": p, "recall": r, "f1": f1}


def grade(answer: str, task: dict) -> dict:
    check = task.get("check")
    items = expect_items(task)
    if not items:
        return {"pass": (check in answer) if check else None}
    tokens = named_tokens(answer)
    names = [n for n, _ in items]
    g = _prf(tokens, names, [])
    g["by_kind"] = {}
    for kind in sorted({k for _, k in items}):
        mine = [n for n, k in items if k == kind]
        g["by_kind"][kind] = _prf(tokens, mine, [n for n in names if n not in mine])
    g["pass"] = g["f1"] >= task.get("pass_f1", 0.8) and (check in answer if check else True)
    return g


def oracle_answer(items: list) -> str:
    return "Answer:\n" + "\n".join(f"- `{n}`" for n, _ in items)


def _mutate(base: str, word: str, before: bool) -> str:
    cut = max(base.rfind("::") + 2 if "::" in base else 0, base.rfind("/") + 1)
    head, last = base[:cut], base[cut:]
    stem, dot, ext = last.rpartition(".") if shape(base) == "path" and "." in last else (last, "", "")
    return head + (f"{word}_{stem}" if before else f"{stem}_{word}") + dot + ext


def decoys(items: list, n: int = 50, seed: int = 0) -> list:
    rng = random.Random(seed)
    names = [x for x, _ in items]
    out = []
    for _ in range(n * 100):
        if len(out) == n:
            break
        d = _mutate(rng.choice(names), rng.choice(DECOY_WORDS) + rng.choice(["", "2", "_all", "_of"]),
                    rng.random() < 0.5)
        if d not in out and not any(matches(d, x) for x in names):
            out.append(d)
    return out


def grader_selftest(spec: dict) -> tuple:
    ok, lines = True, []
    for t in spec["task"]:
        items = expect_items(t)
        if not items:
            lines.append(f"{t['id']}: skip (no expect)")
            continue
        oracle = grade(oracle_answer(items), t)
        empty = grade("", t)
        dump = grade(oracle_answer(items) + "\n" + "\n".join(f"- `{d}`" for d in decoys(items)), t)
        bad = []
        if oracle["f1"] != 1.0 or any(k["f1"] != 1.0 for k in oracle["by_kind"].values()):
            bad.append(f"oracle f1 {oracle['f1']:.3f}")
        if empty["f1"] != 0.0:
            bad.append(f"empty f1 {empty['f1']:.3f}")
        if dump["precision"] >= 0.5:
            bad.append(f"dump precision {dump['precision']:.3f}")
        ok = ok and not bad
        lines.append(f"{t['id']}: " + ("FAIL " + "; ".join(bad) if bad else
                                       f"ok (oracle 1, empty 0, dump precision {dump['precision']:.3f})"))
    return ok, lines


# ---- order and store

def schedule(task_ids: list, harnesses: list, repeat: int, seed: int) -> list:
    rng = random.Random(seed)
    runs = []
    for rep in range(repeat):
        for tid in rng.sample(task_ids, len(task_ids)):
            for h in harnesses:
                arms = ["on", "off"]
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
    return {"cost_usd": j.get("total_cost_usd"), "turns": j.get("num_turns"), "input": u.get("input_tokens"),
            "cache_read": u.get("cache_read_input_tokens"), "cache_write": u.get("cache_creation_input_tokens"),
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
           ("pass", "pass", "{:+.2f}")]


def _num(row: dict, key: str):
    v = row.get(key)
    return float(v) if isinstance(v, (int, float)) else None


def _diff_text(name: str, fmt: str, xs: list, seed: int, draws: int) -> str:
    if not xs:
        return ""
    lo, hi = bootstrap_ci(xs, seed, draws)
    return f"{name} {fmt.format(sum(xs) / len(xs))} [{fmt.format(lo)}, {fmt.format(hi)}]"


def _pass_text(judged: list) -> str:
    if not judged:
        return ""
    k = min(min(len(p["on"]), len(p["off"])) for p in judged)
    out = []
    for kk in sorted({1, k}):
        hat = {arm: sum(pass_hat_k(len(p[arm]), sum(p[arm]), kk) for p in judged) / len(judged)
               for arm in ("on", "off")}
        out.append(f"pass^{kk} on {hat['on']:.2f} off {hat['off']:.2f}")
    return "  ".join(out)


def _p90_text(by_arm: dict) -> str:
    costs = {arm: [c for r in rows if (c := _num(r, "cost_usd")) is not None] for arm, rows in by_arm.items()}
    if not (costs["on"] and costs["off"]):
        return ""
    return f"p90 cost on {percentile(costs['on'], 90):.4f} off {percentile(costs['off'], 90):.4f}"


def report(rows: list, seed: int = 0, draws: int = 10000) -> str:
    groups = defaultdict(lambda: defaultdict(lambda: {"on": {}, "off": {}}))
    for r in rows:
        key = (r["harness"], r.get("headroom", "unknown"), r.get("store", "unknown"))
        groups[key][r["task"]][r["foxdb"]][r.get("rep", 0)] = r
    lines = ["differences are on minus off; brackets are bootstrap 95% CIs (per task over reps, overall over tasks)"]
    for (h, hr, store), tasks in sorted(groups.items()):
        lines.append(f"\n{h} · {hr} · store {store}")
        task_means = defaultdict(list)
        judged = []
        for tid, arms in sorted(tasks.items()):
            reps = sorted(set(arms["on"]) & set(arms["off"]))
            parts = [f"{tid:<24} n {len(reps)}"]
            for name, key, fmt in METRICS:
                xs = [_num(arms["on"][i], key) - _num(arms["off"][i], key) for i in reps
                      if _num(arms["on"][i], key) is not None and _num(arms["off"][i], key) is not None]
                if xs:
                    task_means[name].append(sum(xs) / len(xs))
                parts.append(_diff_text(name, fmt, xs, seed, draws))
            passes = {arm: [r["pass"] for r in arms[arm].values() if isinstance(r.get("pass"), bool)]
                      for arm in ("on", "off")}
            if passes["on"] and passes["off"]:
                judged.append(passes)
                parts.append(_pass_text([passes]))
            parts.append(_p90_text({arm: list(arms[arm].values()) for arm in ("on", "off")}))
            lines.append("  ".join(p for p in parts if p))
        parts = [f"{'ALL':<24} tasks {len(tasks)}"]
        parts += [_diff_text(name, fmt, task_means[name], seed, draws) for name, _, fmt in METRICS]
        parts.append(_pass_text(judged))
        parts.append(_p90_text({arm: [r for t in tasks.values() for r in t[arm].values()] for arm in ("on", "off")}))
        lines.append("  ".join(p for p in parts if p))
    return "\n".join(lines)


# ---- CLI

def run_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("tasks", type=Path)
    ap.add_argument("--harness", choices=["claude", "codex", "both"], default="both")
    ap.add_argument("--fox", default=str(LIVE_HOME / "bin/fox"))
    ap.add_argument("--work", type=Path, default=None, help="scratch dir for repo copies (default: a new temp dir)")
    ap.add_argument("--headroom", choices=["on", "off"], default="off")
    ap.add_argument("--store", choices=["scratch", "live"], default="scratch")
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--seed", type=int, default=None, help="order seed (default: random, recorded)")
    ap.add_argument("--model", default=None, help="model passed to both harnesses (default: theirs)")
    ap.add_argument("--max-usd", type=float, default=1.0, help="per-run cap for claude")
    ap.add_argument("--out", type=Path, default=Path("ab-results.jsonl"))
    ap.add_argument("--grader-selftest", action="store_true", help="check the grader on each task and exit")
    ap.add_argument("--run", action="store_true", help="actually run (default: dry-run, print commands)")
    return ap


def report_main(argv: list) -> int:
    ap = argparse.ArgumentParser(prog="ab.py report")
    ap.add_argument("results", type=Path)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--draws", type=int, default=10000)
    a = ap.parse_args(argv)
    rows = [json.loads(ln) for ln in a.results.read_text().splitlines() if ln.strip()]
    print(report(rows, a.seed, a.draws))
    return 0


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
    seed = a.seed if a.seed is not None else random.SystemRandom().randrange(2**31)
    work = (a.work or Path(tempfile.mkdtemp(prefix="fox-ab-"))).resolve()
    work.mkdir(parents=True, exist_ok=True)
    mode = f"headroom-{a.headroom}"
    fox_sha = sha256_file(a.fox)
    print(f"# {len(tasks)} tasks x {harnesses} x [on, off] x {a.repeat} · {mode} · store {a.store} · seed {seed}"
          f" · fox {fox_sha or 'missing'} · work {work}" + ("" if a.run else " · DRY RUN"))

    prov = {"seed": seed, "store": a.store, "fox_sha256": fox_sha}
    versions = {}
    if a.run:
        prov["repo_commit"] = _out(["git", "-C", str(repo), "rev-parse", "HEAD"])
        prov["bench_commit"] = _out(["git", "-C", str(Path(__file__).resolve().parent), "rev-parse", "HEAD"])
        versions = {h: _out([h, "--version"]) for h in harnesses}
    out = a.out.open("a") if a.run else None
    for slot, s in enumerate(schedule(list(tasks), harnesses, a.repeat, seed)):
        t, h, arm, rep = tasks[s["task"]], s["harness"], s["arm"], s["rep"]
        name = f"{t['id']}-{h}-{arm}-{rep}"
        cwd = work / name
        clone = ["git", "clone", "--quiet", "--shared", str(repo), str(cwd)]
        last = work / f"{name}.last.txt"
        plan = store_plan(a.store, a.fox, cwd, work / f"{name}.home")
        cfg = work / f"{name}.mcp.json"
        if h == "claude":
            cmd = claude_cmd(t["prompt"], cfg, a.max_usd, a.model)
        else:
            cmd = codex_cmd(t["prompt"], cwd, a.fox, arm == "on", last, plan["env"], a.model)
        index = plan["index"] if arm == "on" else None
        if not a.run:
            print(f"# slot {slot}: {name}")
            print(shlex.join(clone))
            if index:
                envs = " ".join(f"{k}={shlex.quote(v)}" for k, v in plan["env"].items())
                print(f"{envs} {shlex.join(index)}")
            print(f"(cd {shlex.quote(str(cwd))} && {shlex.join(cmd)})")
            continue
        cfg.write_text(json.dumps(mcp_config_json(a.fox, arm == "on", plan["env"])))
        if not cwd.exists():
            subprocess.run(clone, check=True)
        env = env_for(a.headroom, plan["env"])
        if index:
            (work / f"{name}.home" / "claude-projects").mkdir(parents=True, exist_ok=True)
            subprocess.run(index, env=env, check=True)
        t0 = time.monotonic()
        p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=1800)
        secs = time.monotonic() - t0
        u = usage_from_claude(p.stdout) if h == "claude" else usage_from_codex(p.stdout, last)
        ans = u.pop("answer", "") or ""
        model = u.pop("model", None) or a.model or "default"
        row = {"task": t["id"], "harness": h, "foxdb": arm, "rep": rep, "slot": slot, "headroom": mode,
               "exit": p.returncode, "secs": round(secs, 1), **u, **grade(ans, t), "model": model,
               "harness_version": versions.get(h), **prov}
        out.write(json.dumps(row) + "\n")
        out.flush()
        print(json.dumps(row))
    return 0


if __name__ == "__main__":
    sys.exit(main())
