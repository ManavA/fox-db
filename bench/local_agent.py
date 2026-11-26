#!/usr/bin/env python3
"""A minimal bash agent for small local models, used by `ab.py --harness local`.

One action per turn: the model replies with one fenced bash block, the loop runs it in the repo
copy and sends back the output (cut at --output-cap characters). A line starting with `FINAL:`
ends the run. Step, token and context caps also end it; after a step or token cap the model gets
one last turn to answer.

Arms:
    grep     bash only
    fox-cli  bash plus a short guide to the `fox` CLI, run against the scratch FOXDB_HOME
    packet   bash only, plus the `fox packet` answer context computed up front: definition, callers
             and 2-hop impact of the symbols the question names (or a `fox find` on the question
             when it names none). --packet-builder python uses the older builder here instead.

Talks to any OpenAI-compatible /v1/chat/completions endpoint (llama.cpp `llama-server`) at
temperature 0. Prints one JSON result line on stdout; --transcript writes every turn as JSON lines.

Server used for the smoke runs (16 GB GPU; --fit keeps the experts that do not fit on the CPU):
    llama-server -m Inkling-2-Mini-IQ4_NL.gguf -c 16384 --fit on -ub 16 -b 16 --jinja -np 1 --temp 0 --port 18431
On llama.cpp b12627 the default micro-batch, and -ub 32, made Inkling-2-Mini emit a run of "G" tokens and
left the server broken until restart, hence -ub 16 -b 16.

Commands run as the user with HOME pointed at --scratch, and with `fox` on PATH only in the fox-cli
arm. That keeps the live store and user config out of reach of a well-behaved command; it is not a
sandbox.

The loop follows SWE-agent (Yang et al. 2024, https://arxiv.org/abs/2405.15793) and
mini-swe-agent (https://github.com/SWE-agent/mini-swe-agent): bash as the only tool, one action
per turn, capped observations.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path

ARMS = ["grep", "fox-cli", "packet"]
OUTPUT_CAP = 2000
LINE_CAP = 400
SHELL_LANGS = {"", "bash", "sh", "shell", "console", "zsh"}
FENCE = re.compile(r"```([\w+-]*)[ \t]*\n(.*?)```", re.S)
FINAL = re.compile(r"^[ \t>*_#]*FINAL:[ \t*_]*", re.M)
THINK = re.compile(r"<think>.*?</think>", re.S)
CODE_NAME = re.compile(r"`([^`\s]+)`|\b([A-Za-z_]\w*(?:::\w+)+|[a-z]\w*_\w+|[A-Z][a-z0-9]+[A-Z]\w*)\b")

FORMAT_ERROR = ("No command found. Reply with exactly one bash command in a ```bash fenced block, "
                "or with a line starting with FINAL: and your answer.")
LAST_CALL = "You are out of turns. Reply now with a line starting with FINAL: and your best answer. No command."

BASE_PROMPT = """You answer questions about the code repository in your current directory. You act through a bash shell, one command per turn.

Each turn, write one short sentence of reasoning, then exactly one bash command in a fenced block:
```bash
grep -rn "parse_config" src | head -40
```
You then see the command's output, cut at {cap} characters. Each command runs from the repo root in a fresh shell, so cd does not carry over. Do not edit files.

Good commands: grep -rn or rg -n to find a name, sed -n '120,170p' FILE to read a span, ls and find to look around. Narrow a search rather than dumping whole files.

When you know the answer, reply with a line that starts with FINAL: followed by the answer, and no code block. Name functions and files exactly as the code spells them, with path:line where you can.

You have at most {steps} turns."""

FOX_GUIDE = """

A code index tool, `fox`, has already indexed this repo:
- fox find WORDS : ranked symbols, files and doc sections with path:line
- fox code NAME --rel callers : call sites grouped by file, then "unresolved mentions" (text hits that may also be calls)
- fox code NAME --rel impact : everything that depends on NAME, transitively
- fox code NAME --rel src : the source of NAME
- fox code NAME --rel callees|refs|outline
If a qualified name (db::open_ro) finds nothing, try its last segment (open_ro). Add --cursor N to page."""

PACKET_INTRO = "Index packet from fox (may be partial; check with grep when unsure):\n"


# ---- parsing and output

def parse_action(text: str) -> tuple:
    """("bash", command), ("final", answer) or ("error", message) for one model reply."""
    text = THINK.sub("", text or "")
    final = FINAL.search(text)
    for m in FENCE.finditer(text):
        if final and m.start() > final.start():
            break
        if m.group(1).lower() in SHELL_LANGS and m.group(2).strip():
            return ("bash", m.group(2).strip())
    if final:
        return ("final", text[final.end():].strip())
    return ("error", FORMAT_ERROR)


def cap_output(text: str, limit: int = OUTPUT_CAP) -> str:
    lines = [ln if len(ln) <= LINE_CAP else ln[:LINE_CAP] + " [line cut]" for ln in text.splitlines()]
    kept, size = [], 0
    for ln in lines:
        if size + len(ln) + 1 > limit:
            break
        kept.append(ln)
        size += len(ln) + 1
    rest = len(lines) - len(kept)
    return "\n".join(kept) + (f"\n[{rest} more lines; narrow your command]" if rest else "")


def run_bash(cmd: str, cwd: Path, env: dict, timeout: int = 60) -> str:
    try:
        p = subprocess.run(["bash", "-c", cmd], cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                           stderr=subprocess.STDOUT, text=True, errors="replace", timeout=timeout)
    except subprocess.TimeoutExpired as e:
        out = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        return out + f"\n[timed out after {timeout}s]"
    out = p.stdout.rstrip("\n")
    if p.returncode:
        out += f"\n[exit {p.returncode}]"
    return out or "[no output]"


# ---- arms

def system_prompt(arm: str, steps: int, cap: int = OUTPUT_CAP) -> str:
    return BASE_PROMPT.format(cap=cap, steps=steps) + (FOX_GUIDE if arm == "fox-cli" else "")


def prompt_names(prompt: str) -> list:
    out = []
    for m in CODE_NAME.finditer(prompt):
        name = (m.group(1) or m.group(2)).strip("().,;:")
        if name and name not in out and re.fullmatch(r"[\w:.]+", name):
            out.append(name)
    return out


def _fox(fox: str, args: list, cwd: Path, env: dict) -> str:
    try:
        p = subprocess.run([fox, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        return ""
    return p.stdout.strip() if p.returncode == 0 else ""


def fox_packet(prompt: str, fox: str, cwd: Path, env: dict, budget: int = 800) -> str:
    """`fox packet` on the prompt."""
    return _fox(fox, ["packet", prompt, "--budget", str(budget)], cwd, env)


def build_packet(prompt: str, fox: str, cwd: Path, env: dict, budget: int = 800) -> str:
    """The older Python builder, kept for comparison: callers (and impact, when callers resolve any
    call) for each name in the prompt, else a find."""
    names = prompt_names(prompt)
    parts = []
    for name in names:
        share = max(120, budget // len(names))
        hit = ""
        for cand in dict.fromkeys([name, re.split(r"::|\.", name)[-1]]):
            hit = _fox(fox, ["code", cand, "--rel", "callers", "--budget", str(share)], cwd, env)
            if hit and not hit.startswith("no symbol matches"):
                break
        if not hit or hit.startswith("no symbol matches"):
            continue
        parts.append(hit)
        if not re.search(r"^0 call sites", hit, re.M):
            impact = _fox(fox, ["code", cand, "--rel", "impact", "--budget", str(share // 2)], cwd, env)
            if impact:
                parts.append(impact)
    if not parts:
        query = re.sub(r"(?i)\bdo not edit files\.?", "", prompt).strip()
        hit = _fox(fox, ["find", query, "--limit", "12", "--budget", str(budget)], cwd, env)
        parts = [hit] if hit else []
    text = "\n\n".join(parts)
    return text[:budget * 4]


def agent_env(arm: str, fox: str, scratch: Path, base: dict) -> dict:
    """HOME in scratch; `fox` on PATH only for fox-cli, via a scratch bin dir holding just that binary."""
    home, bindir = scratch / "home", scratch / "bin"
    home.mkdir(parents=True, exist_ok=True)
    path = [p for p in base.get("PATH", "/usr/bin:/bin").split(":") if p and not (Path(p) / "fox").exists()]
    if arm == "fox-cli":
        bindir.mkdir(parents=True, exist_ok=True)
        link = bindir / "fox"
        if not link.exists():
            link.symlink_to(Path(fox).resolve())
        path.insert(0, str(bindir))
    env = dict(base, HOME=str(home), PATH=":".join(path), PAGER="cat", GIT_PAGER="cat")
    if arm != "fox-cli":
        env["FOXDB_HOME"] = str(scratch / "no-store")
    return env


# ---- model

class ContextFull(Exception):
    pass


def chat(url: str, messages: list, max_tokens: int, model=None, effort=None, timeout: int = 1800) -> dict:
    body = {"messages": messages, "temperature": 0, "max_tokens": max_tokens}
    if model:
        body["model"] = model
    if effort:
        body["chat_template_kwargs"] = {"reasoning_effort": effort}
    req = urllib.request.Request(url.rstrip("/") + "/v1/chat/completions", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            j = json.load(r)
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        if e.code == 400 and re.search(r"context|n_ctx|too long|exceed", detail, re.I):
            raise ContextFull(detail) from e
        raise
    choice = j["choices"][0]
    u = j.get("usage", {})
    return {"content": choice["message"].get("content") or "", "finish": choice.get("finish_reason"),
            "in": u.get("prompt_tokens", 0), "out": u.get("completion_tokens", 0)}


def server_info(url: str) -> dict:
    try:
        with urllib.request.urlopen(url.rstrip("/") + "/props", timeout=10) as r:
            j = json.load(r)
    except (OSError, ValueError):
        return {}
    return {"model": Path(j.get("model_path") or j.get("model_alias") or "").name or None,
            "ctx": j.get("default_generation_settings", {}).get("n_ctx"), "build": j.get("build_info")}


# ---- loop

def elide(messages: list, keep: int = 2) -> list:
    """Blank all but the last `keep` command outputs, to fit a full context for the last call."""
    obs = [i for i, m in enumerate(messages) if m["role"] == "user" and i > 1]
    cut = set(obs[:-keep] if keep else obs)
    return [dict(m, content="[output elided]") if i in cut else m for i, m in enumerate(messages)]

def run_agent(prompt: str, arm: str, cwd: Path, env: dict, url: str, *, packet: str = "", steps: int = 20,
              token_cap: int = 200_000, max_tokens: int = 2048, output_cap: int = OUTPUT_CAP, model=None,
              effort=None, transcript=None) -> dict:
    question = f"Question: {prompt}"
    if packet:
        question += "\n\n" + PACKET_INTRO + packet
    messages = [{"role": "system", "content": system_prompt(arm, steps, output_cap)},
                {"role": "user", "content": question}]
    log = transcript.open("w") if transcript else None

    def note(**kw):
        if log:
            log.write(json.dumps(kw) + "\n")
            log.flush()

    note(role="system", content=messages[0]["content"])
    note(role="user", content=question)
    runs, tin, tout, step, cap, answer, forced, bad = Counter(), 0, 0, 0, None, "", False, 0
    t0 = time.monotonic()
    while True:
        if cap is None and (step >= steps or tin + tout >= token_cap):
            cap = "steps" if step >= steps else "tokens"
            messages.append({"role": "user", "content": LAST_CALL})
            note(role="user", content=LAST_CALL)
        try:
            r = chat(url, messages, max_tokens, model, effort)
        except ContextFull as e:
            note(role="error", content=str(e)[:500])
            if cap == "context":
                break
            cap = "context"
            messages = elide(messages)
            messages.append({"role": "user", "content": LAST_CALL})
            note(role="user", content="[older outputs elided] " + LAST_CALL)
            continue
        step += 1
        tin, tout = tin + r["in"], tout + r["out"]
        messages.append({"role": "assistant", "content": r["content"]})
        kind, val = parse_action(r["content"])
        note(role="assistant", content=r["content"], action=kind, tokens_in=r["in"], tokens_out=r["out"],
             finish=r["finish"])
        if kind == "final":
            answer, forced = val, cap is not None
            break
        if cap:
            answer = THINK.sub("", r["content"])
            break
        if kind == "bash":
            runs[val] += 1
            obs = cap_output(run_bash(val, cwd, env), output_cap)
        else:
            bad += 1
            obs = FORMAT_ERROR if r["finish"] != "length" else "Your reply was cut off. Keep replies short. " + FORMAT_ERROR
        messages.append({"role": "user", "content": obs})
        note(role="user", content=obs)
    loops = {c: n for c, n in runs.items() if n >= 3}
    result = {"arm": arm, "answer": answer, "steps": step, "input": tin, "output": tout,
              "secs": round(time.monotonic() - t0, 1), "cap": cap, "forced_final": forced,
              "commands": sum(runs.values()), "format_errors": bad, "loop": bool(loops), "loop_commands": len(loops),
              "max_repeat": max(runs.values(), default=0), "packet_chars": len(packet)}
    note(role="result", **{k: v for k, v in result.items() if k != "answer"})
    if log:
        log.close()
    return result


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--prompt", required=True)
    ap.add_argument("--arm", choices=ARMS, required=True)
    ap.add_argument("--cwd", type=Path, required=True, help="the repo copy commands run in")
    ap.add_argument("--scratch", type=Path, required=True, help="dir for this run's HOME and bin")
    ap.add_argument("--fox", default=str(Path.home() / ".foxdb/bin/fox"))
    ap.add_argument("--url", default="http://127.0.0.1:18431")
    ap.add_argument("--model", default=None)
    ap.add_argument("--reasoning-effort", default="low", help="chat template kwarg; '' to leave the default")
    ap.add_argument("--steps", type=int, default=20)
    ap.add_argument("--token-cap", type=int, default=200_000, help="prompt plus completion tokens over all turns")
    ap.add_argument("--max-tokens", type=int, default=2048, help="per reply, reasoning included")
    ap.add_argument("--output-cap", type=int, default=OUTPUT_CAP)
    ap.add_argument("--packet-budget", type=int, default=800, help="tokens (about 4 chars each)")
    ap.add_argument("--packet-builder", choices=["fox", "python"], default="fox",
                    help="fox: `fox packet`; python: the older builder in this file")
    ap.add_argument("--transcript", type=Path, default=None)
    a = ap.parse_args(argv)
    env = agent_env(a.arm, a.fox, a.scratch.resolve(), dict(os.environ))
    builder = fox_packet if a.packet_builder == "fox" else build_packet
    packet = builder(a.prompt, a.fox, a.cwd, dict(os.environ), a.packet_budget) if a.arm == "packet" else ""
    res = run_agent(a.prompt, a.arm, a.cwd, env, a.url, packet=packet, steps=a.steps, token_cap=a.token_cap,
                    max_tokens=a.max_tokens, output_cap=a.output_cap, model=a.model,
                    effort=a.reasoning_effort or None, transcript=a.transcript)
    res["server"] = server_info(a.url)
    if a.arm == "packet":
        res["packet_builder"] = a.packet_builder
    print(json.dumps(res))
    return 0


if __name__ == "__main__":
    sys.exit(main())
