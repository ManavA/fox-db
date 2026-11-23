#!/usr/bin/env python3
"""Compiler-oracle tasks: ground truth for "who calls X" and "what stores X as a function value".

For each target function it adds a parameter to the definition (and every declaration the
compiler says conflicts), compiles every translation unit, and classifies each new diagnostic:
`call` (too few arguments), `store` (the function used as a value: initializer, assignment,
return, callback argument) or `other`. The source is restored afterwards, also on error.

Only code the chosen build compiles counts: platform #ifdef branches, files the build skips and
code that never names the function's type (a cast to void *) are invisible to the oracle.

    # C: record the build's compile commands once, then run (no fox or git store is touched
    # outside --fox-home, which must be scratch):
    make -j CC=$PWD/bench/oracle/cc_log.py ORACLE_COMPDB=/scratch/compdb.jsonl ORACLE_REAL_CC=gcc
    python3 bench/oracle/oracle.py c --repo /scratch/repo --compdb /scratch/compdb.jsonl \
        --fox-home /scratch/foxhome --out bench/private/oracle/repo --config "default make, gcc, Linux"
    # Rust (one crate, `cargo check --all-targets`) and Go (`go vet ./...`):
    python3 bench/oracle/oracle.py rust --repo /scratch/crate --fox-home ... --out ...
    python3 bench/oracle/oracle.py go --repo /scratch/module --fox-home ... --out ...

Outputs in --out: tasks.toml (callers-only and stores tasks with `expect`), oracle.json (every
site), comparison.txt (fox callers and non-call refs against the oracle, per kind).
"""

import argparse
import concurrent.futures as cf
import json
import os
import random
import re
import subprocess
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import diag  # noqa: E402
import foxcli  # noqa: E402

MARK = "oracle_extra"


class Skip(Exception):
    pass


# ---------- mutation ----------

def _match(text: str, i: int, open_: str, close: str) -> int:
    depth = 0
    while i < len(text):
        ch = text[i]
        if ch == open_:
            depth += 1
        elif ch == close and not (close == ">" and text[i - 1] == "-"):
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise Skip("unbalanced")


def add_param(text: str, line: int, name: str, lang: str) -> str:
    """`text` with one parameter appended to the parameter list of `name` declared on `line`."""
    starts = [0] + [m.end() for m in re.finditer("\n", text)]
    lo = starts[line - 1]
    hi = starts[min(line + 3, len(starts) - 1)] if line + 3 < len(starts) else len(text)
    m = re.compile(rf"\b{re.escape(name)}\b").search(text, lo, hi)
    if not m:
        raise Skip(f"{name} not on line {line}")
    i = m.end()
    while i < len(text) and text[i].isspace():
        i += 1
    if lang == "rust" and text[i : i + 1] == "<":
        i = _match(text, i, "<", ">") + 1
        while text[i].isspace():
            i += 1
    if text[i : i + 1] != "(":
        raise Skip(f"no parameter list after {name}")
    close = _match(text, i, "(", ")")
    inner = text[i + 1 : close]
    if "..." in inner:
        raise Skip("variadic")
    param = {"c": f"double {MARK}", "rust": f"_{MARK}: f64", "go": f"{MARK} float64"}[lang]
    s = inner.strip()
    if lang == "c" and s in ("", "void"):
        return text[: i + 1] + param + text[close:]
    if s == "":
        return text[: i + 1] + param + text[close:]
    if s.endswith(","):
        return text[:close] + param + text[close:]
    return text[:close] + ", " + param + text[close:]


class Mutation:
    """Edits applied to files, undone by restore() (also on exceptions, via `with`)."""

    def __init__(self, root: Path):
        self.root, self.orig = root, {}

    def edit(self, rel: str, line: int, name: str, lang: str):
        p = self.root / rel
        text = p.read_text(errors="surrogateescape")
        self.orig.setdefault(rel, text)
        p.write_text(add_param(text, line, name, lang), errors="surrogateescape")

    def restore(self):
        for rel, text in self.orig.items():
            (self.root / rel).write_text(text, errors="surrogateescape")
        self.orig = {}

    def __enter__(self):
        return self

    def __exit__(self, *a):
        self.restore()


def leftovers(root: Path, exts: tuple) -> list:
    out = []
    for p in root.rglob("*"):
        if p.suffix in exts and p.is_file():
            try:
                if MARK in p.read_text(errors="ignore"):
                    out.append(str(p))
            except OSError:
                pass
    return out


# ---------- language back ends ----------

class C:
    lang, exts = "c", (".c", ".h")

    def __init__(self, root: Path, compdb: Path, jobs: int):
        self.root, self.jobs = root, jobs
        seen, self.units = set(), []
        for raw in compdb.read_text().splitlines():
            e = json.loads(raw)
            k = (e["directory"], e["file"])
            if k not in seen:
                seen.add(k)
                self.units.append(e)

    def files(self) -> list:
        return sorted({self.rel(e["directory"], e["file"]) for e in self.units})

    def rel(self, d: str, f: str) -> str:
        return os.path.relpath(os.path.normpath(os.path.join(d, f)), self.root)

    def argv(self, e: dict) -> list:
        out, skip = [], False
        for a in e["arguments"]:
            if skip:
                skip = False
                continue
            if a in ("-o", "-MF", "-MT", "-MQ"):
                skip = True
                continue
            if a in ("-c", "-MMD", "-MD", "-MP") or a.startswith("-Werror") and a != "-Werror=incompatible-pointer-types":
                continue
            out.append(a)
        return out + ["-fsyntax-only", "-fdiagnostics-plain-output", "-fmax-errors=0", "-Werror=incompatible-pointer-types"]

    def _one(self, e: dict) -> list:
        r = subprocess.run(self.argv(e), cwd=e["directory"], capture_output=True, text=True, errors="replace",
                           env=dict(os.environ, LC_ALL="C"))
        ds = diag.parse_gcc(r.stderr)
        for d in ds:
            d.file = self.rel(e["directory"], d.file)
            d.notes = [(self.rel(e["directory"], f), ln, c, m) for f, ln, c, m in d.notes]
        return ds

    def compile(self) -> list:
        with cf.ThreadPoolExecutor(self.jobs) as ex:
            return [d for ds in ex.map(self._one, self.units) for d in ds]

    def classify(self, d, name):
        return diag.classify_c(d, name)

    def mutate(self, m: Mutation, name: str, path: str, line: int, base: set) -> list:
        m.edit(path, line, name, "c")
        done = {(path, line)}
        for _ in range(6):
            ds = [d for d in self.compile() if d.key() not in base]
            todo = [x for x in diag.conflicting_decls(ds, name) if x not in done]
            if not todo:
                return ds
            for f, ln in dict.fromkeys(todo):
                m.edit(f, ln, name, "c")
                done.add((f, ln))
        raise Skip("declarations kept conflicting")


class Rust:
    lang, exts = "rust", (".rs",)

    def __init__(self, root: Path, jobs: int):
        self.root, self.jobs = root, jobs

    def files(self) -> list:
        return sorted(str(p.relative_to(self.root)) for p in self.root.rglob("*.rs") if "target" not in p.parts)

    def compile(self) -> list:
        r = subprocess.run(["cargo", "check", "--workspace", "--all-targets", "--keep-going", "--message-format=json", "-q"],
                           cwd=self.root, capture_output=True, text=True)
        ds = diag.parse_rustc_json(r.stdout)
        for d in ds:
            d.file = os.path.relpath(os.path.normpath(os.path.join(self.root, d.file)), self.root)
        return ds

    def classify(self, d, name):
        return diag.classify_rust(d, name)

    def mutate(self, m, name, path, line, base):
        m.edit(path, line, name, "rust")
        return [d for d in self.compile() if d.key() not in base]


class Go:
    lang, exts = "go", (".go",)

    def __init__(self, root: Path, jobs: int):
        self.root, self.jobs = root, jobs
        r = subprocess.run(["go", "list", "./..."], cwd=root, capture_output=True, text=True, check=True)
        self.pkgs = r.stdout.split()

    def files(self) -> list:
        return sorted(str(p.relative_to(self.root)) for p in self.root.rglob("*.go"))

    def _one(self, cmd: list) -> str:
        r = subprocess.run(cmd, cwd=self.root, capture_output=True, text=True)
        return r.stdout + r.stderr

    def compile(self) -> list:
        """Every package, then each package with its tests: `go test ./...` stops a package's test
        build once the plain build fails, so test files need their own `go test -c`."""
        cmds = [["go", "build", "-gcflags=-e", "./..."]]
        cmds += [["go", "test", "-c", "-o", os.devnull, "-gcflags=-e", p] for p in self.pkgs]
        seen, out = set(), []
        with cf.ThreadPoolExecutor(self.jobs) as ex:
            for text in ex.map(self._one, cmds):
                for d in diag.parse_go(text):
                    d.file = os.path.relpath(os.path.normpath(os.path.join(self.root, d.file)), self.root)
                    if d.key() not in seen:
                        seen.add(d.key())
                        out.append(d)
        return out

    def classify(self, d, name):
        return diag.classify_go(d, name)

    def mutate(self, m, name, path, line, base):
        m.edit(path, line, name, "go")
        ds = [d for d in self.compile() if d.key() not in base]
        for d in ds:
            if diag.classify_go(d, name) == "call":
                d.line = call_line((self.root / d.file).read_bytes(), d.line, d.col, name)
        return ds


def call_line(src: bytes, line: int, col: int, name: str) -> int:
    """Line where the innermost call to `name` holding line:col starts (go reports its last argument)."""
    starts = [0] + [i + 1 for i, b in enumerate(src) if b == 10]
    end = starts[line - 1] + col - 1
    best = line
    for m in re.finditer(rb"\b" + re.escape(name.encode()) + rb"\s*\(", src[:end]):
        i, depth, q = m.end() - 1, 0, None
        while i < len(src):
            c = src[i : i + 1]
            if q:
                if c == b"\\" and q != b"`":
                    i += 1
                elif c == q:
                    q = None
            elif c in b"\"'`":
                q = c
            elif c == b"(":
                depth += 1
            elif c == b")":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        if m.start() <= end <= i:
            best = src.count(b"\n", 0, m.start()) + 1
    return best


# ---------- naming sites ----------

RUST_CHAR = re.compile(r"'(?:\\u\{[0-9a-fA-F]+\}|\\.|[^\\'\n])'")
_blocks: dict = {}


def rust_blocks(text: str) -> list:
    """Per line (index 0 = line 1): the header of the innermost `{` block open at the line's start.
    Skips comments, strings, raw strings and char literals."""
    out, stack, seg, i, n = [""], [], 0, 0, len(text)
    while i < n:
        c = text[i]
        if c == "\n":
            out.append(stack[-1] if stack else "")
        elif text.startswith("//", i):
            i = text.find("\n", i) - 1 if text.find("\n", i) >= 0 else n
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                depth += text.startswith("/*", i) - text.startswith("*/", i)
                if text[i] == "\n":
                    out.append(stack[-1] if stack else "")
                i += 2 if text.startswith(("/*", "*/"), i) else 1
            continue
        elif (m := re.compile(r'b?r(#*)"').match(text, i)) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            end = text.find('"' + m[1], m.end())
            end = n if end < 0 else end + 1 + len(m[1])
            out += [stack[-1] if stack else ""] * text.count("\n", i, end)
            i = end
            continue
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out += [stack[-1] if stack else ""] * text.count("\n", i, j)
            i = j + 1
            continue
        elif c == "'" and (m := RUST_CHAR.match(text, i)):
            i = m.end()
            continue
        elif c == "{":
            stack.append(" ".join(re.sub(r"//[^\n]*|/\*.*?\*/", "", text[seg:i], flags=re.S).split()))
            seg = i + 1
        elif c == "}":
            if stack:
                stack.pop()
            seg = i + 1
        elif c == ";":
            seg = i + 1
        i += 1
    return out


def rust_container(src: list, line: int) -> str:
    """The `impl`, `trait` or `mod` header of the block holding a Rust fn on `line`, or ""."""
    key = hash("\n".join(src))
    if key not in _blocks:
        _blocks[key] = rust_blocks("\n".join(src))
    heads = _blocks[key]
    head = heads[line - 1] if 0 < line <= len(heads) else ""
    head = re.sub(r"^(?:#!?\[[^\]]*\]\s*)+", "", head)
    return head if re.match(r"(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:impl|mod|trait)\b", head) else ""


def skip_generics(s: str) -> str:
    s = s.lstrip()
    if not s.startswith("<"):
        return s
    depth = 0
    for i, ch in enumerate(s):
        depth += (ch == "<") - (ch == ">" and s[i - 1] != "-")
        if depth == 0:
            return s[i + 1 :]
    return ""


def qualify(lang: str, src: list, line: int, name: str) -> str:
    """`name` as fox's graph names the definition on `line`: Type.name for a Go method, and for a
    Rust fn in an impl, trait or mod, that container's name."""
    here = src[line - 1] if 0 < line <= len(src) else ""
    if lang == "go":
        m = re.match(r"\s*func\s*\(\s*(?:\w+\s+)?\*?\s*(\w+)", here)
        return f"{m[1]}.{name}" if m else name
    if lang != "rust":
        return name
    head = rust_container(src, line)
    m = re.match(r"(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(impl|mod|trait)\b(.*)", head)
    if not m:
        return name
    rest = skip_generics(m[2]) if m[1] == "impl" else m[2]
    rest = re.split(r"\bfor\b", rest)[-1]
    ty = re.match(r"\s*(?:&\s*)?(?:dyn\s+)?(?:\w+::)*(\w+)", rest)
    return f"{ty[1]}.{name}" if ty else name


class Namer:
    """Site -> enclosing function (fox outline), or the variable a file-scope initializer fills."""

    def __init__(self, fox: foxcli.Fox, root: Path, lang: str):
        self.fox, self.root, self.lang, self.cache, self.src = fox, root, lang, {}, {}

    def lines(self, path: str) -> list:
        if path not in self.src:
            try:
                self.src[path] = (self.root / path).read_text(errors="replace").splitlines()
            except OSError:
                self.src[path] = []
        return self.src[path]

    def code(self, path: str, line: int) -> str:
        ls = self.lines(path)
        return ls[line - 1].strip()[:140] if 0 < line <= len(ls) else ""

    def top_var(self, path: str, line: int) -> str:
        ls = self.lines(path)
        for i in range(min(line, len(ls)) - 1, -1, -1):
            s = ls[i]
            if s and not s[0].isspace() and s[0] not in "}#/*":
                m = re.search(r"([A-Za-z_]\w*)\s*(?:\[[^\]]*\]\s*)*=", s)
                if m:
                    return m[1]
                break
        return f"(top {Path(path).name})"

    def name(self, path: str, line: int, compiler_func: str | None = None) -> str:
        if compiler_func:
            return compiler_func
        if path not in self.cache:
            self.cache[path] = self.fox.outline(path)
        sym = foxcli.enclosing(self.cache[path], line)
        return qualify(self.lang, self.lines(path), sym.line, sym.name) if sym else self.top_var(path, line)


# ---------- targets ----------

IDENT = re.compile(r"\b([A-Za-z_]\w*)\b\s*\(")


def local_only(be, path: str, line: int, name: str) -> bool:
    """Whether every user of the function compiles in the same unit as it does: Rust items that are
    not plain `pub`, unexported Go functions. A broken crate or package hides its dependents."""
    if be.lang == "c":
        return True
    if be.lang == "go":
        return name[:1].islower()
    src = (be.root / path).read_text(errors="ignore").splitlines()
    text = src[line - 1] if 0 < line <= len(src) else ""
    return not re.match(r"\s*pub(\s|$)", text)


def in_trait(src: list, line: int) -> bool:
    """Whether the Rust fn on `line` sits in a trait or a trait impl (`impl X for Y`), whose
    signature the trait fixes."""
    head = rust_container(src, line)
    return bool(re.match(r"(?:unsafe\s+)?(?:impl\b.*\bfor\b|(?:pub(?:\([^)]*\))?\s+)?trait\b)", head))


def defs_of(be, fox) -> dict:
    """Query name -> [(path, line)] of function definitions in fox's outlines; methods as Type.name."""
    defs = defaultdict(list)
    for f in be.files():
        if be.lang == "c" and f.endswith(".h"):
            continue
        src = (be.root / f).read_text(errors="ignore").splitlines()
        for s in fox.outline(f):
            if s.kind not in foxcli.FN_KINDS or be.lang == "rust" and in_trait(src, s.line):
                continue
            defs[qualify(be.lang, src, s.line, s.name)].append((f, s.line))
    return defs


def pick_targets(be, fox: foxcli.Fox, n: int, seed: int, lo: int, hi: int, log) -> list:
    """Functions fox's graph gives `lo`..`hi` distinct callers; half of them with non-call refs in fox's graph."""
    defs = defs_of(be, fox)
    calls, macros = Counter(), set()
    for f in be.files() + ([str(p.relative_to(be.root)) for p in be.root.rglob("*.h")] if be.lang == "c" else []):
        try:
            text = (be.root / f).read_text(errors="ignore")
        except OSError:
            continue
        macros.update(re.findall(r"#\s*define\s+([A-Za-z_]\w*)", text))
        calls.update(IDENT.findall(text))
    pool = sorted(q for q, ds in defs.items() if len(ds) == 1 and q.split(".")[-1] not in macros
                  and lo + 1 <= calls[q.split(".")[-1]] <= hi * 4 and len(q.split(".")[-1]) > 3
                  and not q.split(".")[-1].startswith(("main", "test")) and local_only(be, *ds[0], q.split(".")[-1]))
    random.Random(seed).shuffle(pool)
    want = {True: n - n // 2, False: n // 2}
    got = {True: [], False: []}
    for q in pool:
        if len(got[True]) >= want[True] and len(got[False]) >= n - min(len(got[True]), want[True]):
            break
        path, line = defs[q][0]
        g = fox.graph(q, "callers")
        if g.target != f"{path}:{line}":
            continue
        who = {(s.path, s.who) for s in g.sites}
        if not lo <= len(who) <= hi:
            continue
        at = {(s.path, s.line) for s in g.sites} | {(path, line)}
        values = len({(s.path, s.line) for s in fox.graph(q, "refs").sites} - at)
        got[values > 0].append({"name": q, "path": path, "line": line, "fox_callers": len(who), "fox_value_refs": values})
    # Half with non-call refs when the repo has them; the rest of the n from the other half.
    picked = got[True][: want[True]]
    picked += got[False][: n - len(picked)]
    for t in picked:
        log(f"target {t['name']} {t['path']}:{t['line']} fox callers={t['fox_callers']} non-call refs={t['fox_value_refs']}")
    return picked


# ---------- oracle run ----------

def run_oracle(be, targets: list, namer: Namer, log) -> list:
    t0 = time.time()
    base_diags = be.compile()
    base = {d.key() for d in base_diags}
    log(f"baseline: {len(base_diags)} diagnostics in {time.time() - t0:.0f}s")
    results = []
    for t in targets:
        t0 = time.time()
        bare = t["name"].split(".")[-1]
        try:
            with Mutation(be.root) as m:
                ds = be.mutate(m, bare, t["path"], t["line"], base)
        except Skip as e:
            log(f"skip {t['name']}: {e}")
            continue
        sites = {}
        for d in ds:
            if d.sev not in ("error", "warning", "fatal error"):
                continue
            kind = be.classify(d, bare)
            if kind == "decl":
                continue
            f, ln = d.site()
            if (f, ln) == (t["path"], t["line"]):
                continue
            k = (f, ln)
            prev = sites.get(k)
            if prev and prev["kind"] != "other":
                continue
            sites[k] = {"path": f, "line": ln, "kind": kind, "msg": d.msg[:200],
                        "func": namer.name(f, ln, d.func), "code": namer.code(f, ln)}
        if not sites:
            log(f"skip {t['name']}: the change broke nothing, so no caller (or the definition) compiles in this config")
            continue
        r = dict(t, sites=sorted(sites.values(), key=lambda s: (s["path"], s["line"])))
        c = Counter(s["kind"] for s in r["sites"])
        log(f"{t['name']}: {dict(c)} in {time.time() - t0:.0f}s")
        results.append(r)
    return results


# ---------- fox vs oracle ----------

def fox_sites(fox, namer, r) -> dict:
    callers = fox.graph(r["name"], "callers")
    refs = fox.graph(r["name"], "refs")
    call_at = {(s.path, s.line) for s in callers.sites}

    def nm(s):
        return namer.name(s.path, s.line) if s.who == "(top)" else s.who

    calls = {(s.path, s.line): (nm(s), s.guess) for s in callers.sites}
    vals = {(s.path, s.line): (nm(s), s.guess) for s in refs.sites if (s.path, s.line) not in call_at
            and (s.path, s.line) != (r["path"], r["line"])}
    return {"calls": calls, "values": vals, "resolved": callers.target}


def pr(tp, fp, fn) -> str:
    p = f"{tp / (tp + fp):.2f}" if tp + fp else "-"
    rc = f"{tp / (tp + fn):.2f}" if tp + fn else "-"
    return f"precision {p} ({tp}/{tp + fp}), recall {rc} ({tp}/{tp + fn})"


def compare(results, fox, namer, config: str) -> tuple[str, list]:
    out = [f"fox callers vs compiler oracle. Config: {config}.",
           "Only code compiled in that config counts; sites the build skips are absent from the oracle.",
           "call = fox `--rel callers` sites vs oracle too-few-argument sites.",
           "store = fox non-call refs (`--rel refs` minus callers) vs oracle function-pointer sites; fox non-call",
           "refs that the oracle saw as `other` (comparisons, casts) count as neither hit nor miss.",
           ""]
    tot = Counter()
    rows = []
    for r in results:
        fs = fox_sites(fox, namer, r)
        o_call = {(s["path"], s["line"]): s for s in r["sites"] if s["kind"] == "call"}
        o_store = {(s["path"], s["line"]): s for s in r["sites"] if s["kind"] == "store"}
        o_other = {(s["path"], s["line"]) for s in r["sites"] if s["kind"] == "other"}
        f_call, f_val = fs["calls"], fs["values"]
        res = {}
        for kind, o, f in (("call", o_call, f_call), ("store", o_store, {k: v for k, v in f_val.items() if k not in o_other})):
            tp, fp, fn = len(o.keys() & f.keys()), len(f.keys() - o.keys()), len(o.keys() - f.keys())
            of = {(k[0], v["func"]) for k, v in o.items()}
            ff = {(k[0], v[0]) for k, v in f.items()}
            ftp, ffp, ffn = len(of & ff), len(ff - of), len(of - ff)
            tot.update({f"{kind}_tp": tp, f"{kind}_fp": fp, f"{kind}_fn": fn,
                        f"{kind}_ftp": ftp, f"{kind}_ffp": ffp, f"{kind}_ffn": ffn})
            res[kind] = (tp, fp, fn, ftp, ffp, ffn, sorted(f.keys() - o.keys()), sorted(o.keys() - f.keys()), f, o)
        rows.append((r, fs, res))
    out.append("TOTAL over %d targets" % len(results))
    for kind in ("call", "store"):
        out.append(f"  {kind} sites:     " + pr(tot[f'{kind}_tp'], tot[f'{kind}_fp'], tot[f'{kind}_fn']))
        out.append(f"  {kind} functions: " + pr(tot[f'{kind}_ftp'], tot[f'{kind}_ffp'], tot[f'{kind}_ffn']))
    out.append("")
    for r, fs, res in rows:
        out.append(f"== {r['name']}  {r['path']}:{r['line']}  (fox resolved {fs['resolved'] or 'nothing'})")
        for kind in ("call", "store"):
            tp, fp, fn, ftp, ffp, ffn, fps, fns, f, o = res[kind]
            if not (tp or fp or fn):
                out.append(f"  {kind}: none either side")
                continue
            out.append(f"  {kind} sites: {pr(tp, fp, fn)}; functions: {pr(ftp, ffp, ffn)}")
            for k in fps:
                g = " (fox marks guess)" if f[k][1] else ""
                out.append(f"    fox only  {k[0]}:{k[1]} {f[k][0]}{g} | {namer.code(*k)}")
            for k in fns:
                out.append(f"    oracle only {k[0]}:{k[1]} {o[k]['func']} | {o[k]['code']}")
            for k in sorted(o.keys() & f.keys()):
                if o[k]["func"] != f[k][0]:
                    out.append(f"    same site, fox names {f[k][0]}, compiler names {o[k]['func']}: {k[0]}:{k[1]}")
        others = [s for s in r["sites"] if s["kind"] == "other"]
        for s in others:
            out.append(f"  other {s['path']}:{s['line']} {s['func']} | {s['msg'][:100]}")
    return "\n".join(out) + "\n", rows


# ---------- tasks ----------

def q(s: str) -> str:
    return json.dumps(s, ensure_ascii=False)


def tasks_toml(results, repo: str, config: str, lang: str) -> str:
    where = f"Only code compiled in this build counts ({config}); ignore code the build does not compile."
    out = [f"# Compiler-oracle tasks generated by bench/oracle/oracle.py. {where}", f"repo = {q(repo)}", ""]
    for r in results:
        calls = sorted({(s["func"], s["path"]) for s in r["sites"] if s["kind"] == "call"})
        stores = sorted({(s["func"], s["path"]) for s in r["sites"] if s["kind"] == "store"})
        at = f"`{r['name']}` (defined at {r['path']}:{r['line']})"
        if calls:
            out += ["[[task]]", f"id = {q('oracle-callers-' + r['name'])}",
                    "prompt = " + q(f"List every function in this repo that directly calls {at}. Count call expressions only, "
                                    "including a call written through a macro (name the function where the macro is used). "
                                    "Do not count places that store it in a function pointer, pass it as a callback, or put it "
                                    f"in a table. {where} Answer with one line per caller: file path and function name. "
                                    "Do not edit files."),
                    "expect = [" + ", ".join(f"{{name = {q(n)}, file = {q(p)}, kind = \"call\"}}" for n, p in calls) + "]", ""]
        if stores:
            out += ["[[task]]", f"id = {q('oracle-stores-' + r['name'])}",
                    "prompt = " + q(f"List every place in this repo that uses {at} as a value without calling it: stores it "
                                    "in a variable or struct field, puts it in a table initializer, passes it as a callback "
                                    "argument, or returns it. Direct calls do not count. Name the enclosing function, or the "
                                    f"variable for a file-scope initializer. {where} Answer with one line per place: file path "
                                    "and function or variable name. Do not edit files."),
                    "expect = [" + ", ".join(f"{{name = {q(n)}, file = {q(p)}, kind = \"store\"}}" for n, p in stores) + "]", ""]
    return "\n".join(out)


def head_commit(root: Path) -> str:
    g = root / ".git"
    try:
        h = (g / "HEAD").read_text().strip()
        if h.startswith("ref: "):
            ref = h[5:]
            p = g / ref
            if p.exists():
                return p.read_text().strip()
            for ln in (g / "packed-refs").read_text().splitlines():
                if ln.endswith(" " + ref):
                    return ln.split()[0]
        return h
    except OSError:
        return "unknown"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("lang", choices=["c", "rust", "go"])
    ap.add_argument("--repo", type=Path, required=True)
    ap.add_argument("--compdb", type=Path, help="C: JSON lines from cc_log.py")
    ap.add_argument("--fox", default=os.path.expanduser("~/.foxdb/bin/fox"))
    ap.add_argument("--fox-home", required=True, help="scratch FOXDB_HOME (never ~/.foxdb)")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--config", default="", help="the build configuration, stated in every prompt")
    ap.add_argument("--task-repo", help="repo path written into tasks.toml (default --repo)")
    ap.add_argument("--targets", help="comma-separated names instead of automatic picks")
    ap.add_argument("-n", type=int, default=20)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--min-callers", type=int, default=3)
    ap.add_argument("--max-callers", type=int, default=40)
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--no-index", action="store_true", help="skip `fox index` (store already current)")
    a = ap.parse_args()
    root = a.repo.resolve()
    a.out.mkdir(parents=True, exist_ok=True)
    logf = (a.out / "log.txt").open("a")

    def log(s):
        print(s, flush=True)
        logf.write(s + "\n")
        logf.flush()

    be = {"c": lambda: C(root, a.compdb, a.jobs), "rust": lambda: Rust(root, a.jobs), "go": lambda: Go(root, a.jobs)}[a.lang]()
    left = leftovers(root, be.exts)
    if left:
        raise SystemExit(f"{MARK} already in {left[:5]}: restore these files first")
    config = a.config or {"c": "compile commands in --compdb", "rust": "cargo check --all-targets", "go": "go vet ./..."}[a.lang]
    fox = foxcli.Fox(a.fox, a.fox_home, str(root))
    if not a.no_index:
        log(fox.index(str(root)).strip())
    namer = Namer(fox, root, a.lang)
    if a.targets:
        targets = []
        for nm in a.targets.split(","):
            g = fox.graph(nm, "callers")
            path, line = g.target.rsplit(":", 1)
            targets.append({"name": nm, "path": path, "line": int(line), "fox_callers": len({(s.path, s.who) for s in g.sites})})
    else:
        targets = pick_targets(be, fox, a.n, a.seed, a.min_callers, a.max_callers, log)
    results = run_oracle(be, targets, namer, log)
    left = leftovers(root, be.exts)
    if left:
        log(f"WARNING: {MARK} left in {left}")
    meta = {"lang": a.lang, "config": config, "commit": head_commit(root), "fox": fox.run("--version").strip()}
    (a.out / "oracle.json").write_text(json.dumps({"meta": meta, "targets": results}, indent=1))
    (a.out / "tasks.toml").write_text(tasks_toml(results, a.task_repo or str(a.repo), config, a.lang))
    text, _ = compare(results, fox, namer, f"{config}; commit {meta['commit'][:12]}; {meta['fox']}")
    (a.out / "comparison.txt").write_text(text)
    print(text)


if __name__ == "__main__":
    main()
