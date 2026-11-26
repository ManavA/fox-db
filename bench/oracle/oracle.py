#!/usr/bin/env python3
"""Compiler-oracle tasks: ground truth for "who calls X" and "what stores X as a function value".

For each target function it adds a parameter to the definition (and every declaration the
compiler says conflicts), compiles every unit of the build, and classifies each new diagnostic:
`call` (too few arguments to the target), `store` (the target used as a value: initializer,
assignment, return, callback argument) or `other`. Files are restored byte for byte afterwards,
also on an exception, Ctrl-C, SIGTERM or SIGHUP.

Targets, their definitions and every enclosing-function name come from the source and the
compiler, never from fox: a C brace scan of the compiled units, a Rust brace scan, go/ast for Go
(goast/main.go). Fox is only queried to be compared, and its sites in code the build does not
compile (files outside the build, inactive #if branches, cfg'd-off Rust items) are counted apart.

Only code the chosen build compiles counts: other platforms' branches, files the build skips and
code that never names the function's type (a cast to void *; in Rust `let g = S::foo;` with
the fn item type inferred) are invisible to the oracle.

    # C: record the build's compile commands once, then run (--fox-home must be scratch):
    make -j CC=$PWD/bench/oracle/cc_log.py ORACLE_COMPDB=/scratch/compdb.jsonl ORACLE_REAL_CC=gcc
    python3 bench/oracle/oracle.py c --repo /scratch/repo --compdb /scratch/compdb.jsonl \
        --fox-home /scratch/foxhome --out bench/private/oracle/repo --config "default make, gcc, Linux"
    # Rust workspace (`cargo check --workspace --all-targets`) and Go module (`go build`, `go test -c`):
    python3 bench/oracle/oracle.py rust --repo /scratch/crate --fox-home ... --out ...
    python3 bench/oracle/oracle.py go --repo /scratch/module --fox-home ... --out ...

With --depth 2 (the default) it also changes every direct caller of each target and builds
two-hop impact tasks: the direct callers plus their own direct callers, each item tagged with its
hop. Every expected item records fox's own label for it (certain, guess or missing, from `fox code
--rel callers|refs|impact` on the scratch store), and each task lists in `fox_extra` what fox
answers that the oracle says is wrong; bench/ab.py report reads both. See bench/oracle/README.md.

Outputs in --out: tasks.toml (callers, stores and impact tasks with `depth` and `expect`), oracle.json (every
site), comparison.txt (fox callers and non-call refs against the oracle, per kind, plus fox's
misses on definitions).
"""

import argparse
import concurrent.futures as cf
import json
import os
import random
import re
import signal
import subprocess
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import diag  # noqa: E402
import foxcli  # noqa: E402
import scan  # noqa: E402

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
    starts = scan.line_starts(text)
    if not 0 < line <= len(starts):
        raise Skip(f"no line {line}")
    lo = starts[line - 1]
    hi = starts[line + 3] if line + 3 < len(starts) else len(text)
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
    if s == "" or lang == "c" and s == "void":
        return text[: i + 1] + param + text[close:]
    if s.endswith(","):
        return text[:close] + param + text[close:]
    return text[:close] + ", " + param + text[close:]


ACTIVE: list = []  # mutations not yet restored, for the signal handler


class Mutation:
    """Edits to files under `root`, undone byte for byte by restore()."""

    def __init__(self, root: Path):
        self.root, self.orig = root.resolve(), {}

    def edit(self, rel: str, line: int, name: str, lang: str):
        p = (self.root / rel).resolve()
        if not p.is_relative_to(self.root):
            raise Skip(f"{rel} is outside the repo")
        raw = p.read_bytes()
        self.orig.setdefault(p, raw)
        p.write_bytes(add_param(raw.decode("utf-8", "surrogateescape"), line, name, lang).encode("utf-8", "surrogateescape"))

    def restore(self):
        for p, raw in self.orig.items():
            p.write_bytes(raw)
        self.orig = {}

    def __enter__(self):
        ACTIVE.append(self)
        return self

    def __exit__(self, *a):
        self.restore()
        ACTIVE.remove(self)


def restore_on_signal(signum, _frame):
    for m in list(ACTIVE):
        m.restore()
    raise SystemExit(128 + signum)


def pool_map(fn, items, jobs: int) -> list:
    """`fn` over `items` on threads; an interrupt cancels the queued calls."""
    ex = cf.ThreadPoolExecutor(jobs)
    try:
        return list(ex.map(fn, items))
    except BaseException:
        ex.shutdown(wait=False, cancel_futures=True)
        raise
    finally:
        ex.shutdown(wait=True)


def leftovers(root: Path, exts: tuple) -> list:
    out = []
    for p in root.rglob("*"):
        if p.suffix in exts and p.is_file() and MARK.encode() in p.read_bytes():
            out.append(str(p))
    return out


def read(root: Path, rel: str) -> str:
    try:
        return (root / rel).read_bytes().decode("utf-8", "replace")
    except OSError:
        return ""


# ---------- language back ends ----------

class Backend:
    """Per language: compile, classify, the build's files and inactive lines, definitions, names."""

    lang, exts = "", ()

    def __init__(self, root: Path, jobs: int):
        self.root, self.jobs = root.resolve(), jobs
        self._defs: dict = {}
        self._src: dict = {}

    def rel(self, d: str, f: str) -> str:
        return os.path.relpath(os.path.normpath(os.path.join(d, f)), self.root)

    def text(self, rel: str) -> str:
        if rel not in self._src:
            self._src[rel] = read(self.root, rel)
        return self._src[rel]

    def defs_in(self, rel: str) -> list:
        if rel not in self._defs:
            self._defs[rel] = self.scan_defs(rel)
        return self._defs[rel]

    def enclosing(self, rel: str, line: int, d=None) -> str | None:
        f = d.func_at(rel) if d is not None else None
        if f:
            return f
        best = scan.innermost(self.defs_in(rel), line)
        return best.name if best else None

    def top_name(self, rel: str, line: int) -> str | None:
        """The variable a file-scope initializer on `line` fills, or None."""
        ls = self.text(rel).split("\n")
        pat = {"c": r"([A-Za-z_]\w*)\s*(?:\[[^\]]*\]\s*)*=",
               "rust": r"^(?:pub(?:\([^)]*\))?\s+)?(?:static|const)\s+(?:mut\s+)?([A-Za-z_]\w*)",
               "go": r"^var\s+([A-Za-z_]\w*)"}[self.lang]
        for i in range(min(line, len(ls)) - 1, -1, -1):
            s = ls[i]
            if s and not s[0].isspace() and s[0] not in "}#/*)":
                m = re.search(pat, s)
                return m[1] if m else None
        return None

    def in_macro_def(self, rel: str, line: int) -> bool:
        return False

    def inactive(self) -> tuple[set, dict]:
        """(files the build compiles, file -> lines its configuration leaves out)."""
        raise NotImplementedError

    def local_only(self, d) -> bool:
        return True


class C(Backend):
    lang, exts = "c", (".c", ".h")

    def __init__(self, root: Path, compdb: Path, jobs: int):
        super().__init__(root, jobs)
        seen, self.units = set(), []
        for raw in compdb.read_text().splitlines():
            e = json.loads(raw)
            k = (e["directory"], e["file"])
            if k not in seen:
                seen.add(k)
                self.units.append(e)

    def sources(self) -> list:
        return sorted({self.rel(e["directory"], e["file"]) for e in self.units})

    def scan_defs(self, rel: str) -> list:
        return scan.c_functions(self.text(rel))

    def base_argv(self, e: dict) -> list:
        out, skip = [], False
        for a in e["arguments"]:
            if skip:
                skip = False
                continue
            if a in ("-o", "-MF", "-MT", "-MQ"):
                skip = True
                continue
            if a in ("-c", "-MMD", "-MD", "-MP") or a.startswith("-Werror"):
                continue
            out.append(a)
        return out

    def _one(self, e: dict) -> list:
        argv = self.base_argv(e) + ["-fsyntax-only", "-fdiagnostics-plain-output", "-fmax-errors=0",
                                    "-Werror=incompatible-pointer-types"]
        r = subprocess.run(argv, cwd=e["directory"], capture_output=True, text=True, errors="replace",
                           env=dict(os.environ, LC_ALL="C"))
        ds = diag.parse_gcc(r.stderr)
        for d in ds:
            d.file = self.rel(e["directory"], d.file)
            d.notes = [(self.rel(e["directory"], f), ln, c, m) for f, ln, c, m in d.notes]
            d.exp = [(self.rel(e["directory"], f), ln) for f, ln in d.exp]
            if d.ctx:
                d.ctx = (self.rel(e["directory"], d.ctx[0]), d.ctx[1])
        return ds

    def compile(self) -> list:
        return [d for ds in pool_map(self._one, self.units, self.jobs) for d in ds]

    def classify(self, d, name):
        return diag.classify_c(d, name)

    def in_macro_def(self, rel: str, line: int) -> bool:
        return scan.c_in_define(self.text(rel), line)

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

    def _pp(self, e: dict) -> dict:
        r = subprocess.run(self.base_argv(e) + ["-E"], cwd=e["directory"], capture_output=True, text=True,
                           errors="replace", env=dict(os.environ, LC_ALL="C"))
        return {self.rel(e["directory"], f): ls for f, ls in scan.preprocessed_lines(r.stdout, str(self.root)).items()}

    def inactive(self):
        active = defaultdict(set)
        for got in pool_map(self._pp, self.units, self.jobs):
            for f, ls in got.items():
                active[f] |= ls
        files = {f for f in active if not f.startswith("..")}
        return files, {f: scan.inactive_lines(self.text(f), active[f]) for f in files}


class Rust(Backend):
    lang, exts = "rust", (".rs",)

    def __init__(self, root: Path, jobs: int):
        super().__init__(root, jobs)
        meta = subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=root,
                              capture_output=True, text=True, check=True)
        self.target_dir = Path(json.loads(meta.stdout)["target_directory"])
        self._files = None

    def scan_defs(self, rel: str) -> list:
        return scan.rust_fns(self.text(rel))

    def compiled(self) -> set:
        """Source files in the dep-info (.d) files of the last check."""
        if self._files is None:
            out = set()
            for d in (self.target_dir / "debug" / "deps").glob("*.d"):
                for tok in re.split(r"(?<!\\)\s+", d.read_text(errors="replace")):
                    tok = tok.rstrip(":").replace("\\ ", " ")
                    if not tok.endswith(".rs"):
                        continue
                    rel = os.path.relpath(os.path.join(self.root, tok), self.root)
                    if not rel.startswith(".."):
                        out.add(rel)
            self._files = out
        return self._files

    def sources(self) -> list:
        return sorted(self.compiled())

    def compile(self) -> list:
        r = subprocess.run(["cargo", "check", "--workspace", "--all-targets", "--keep-going", "--message-format=json", "-q"],
                           cwd=self.root, capture_output=True, text=True)
        ds = diag.parse_rustc_json(r.stdout)
        for d in ds:
            d.file = self.rel(str(self.root), d.file)
            d.exp = [(self.rel(str(self.root), f), ln) for f, ln in d.exp]
        return ds

    def classify(self, d, name):
        return diag.classify_rust(d, name)

    def in_macro_def(self, rel: str, line: int) -> bool:
        return any(a <= line <= b for a, b in scan.rust_macro_defs(self.text(rel)))

    def mutate(self, m, name, path, line, base):
        m.edit(path, line, name, "rust")
        return [d for d in self.compile() if d.key() not in base]

    def inactive(self):
        files = self.compiled()
        return files, {f: scan.rust_cfg_off(self.text(f)) for f in files}

    def local_only(self, d) -> bool:
        return not d.public and not d.trait


class Go(Backend):
    lang, exts = "go", (".go",)

    def __init__(self, root: Path, jobs: int, out: Path):
        super().__init__(root, jobs)
        fmt = "{{.Dir}}|{{join .GoFiles \",\"}},{{join .CgoFiles \",\"}},{{join .TestGoFiles \",\"}},{{join .XTestGoFiles \",\"}}"
        r = subprocess.run(["go", "list", "-f", fmt, "./..."], cwd=root, capture_output=True, text=True, check=True)
        self.pkgs, files = [], set()
        for ln in r.stdout.splitlines():
            d, names = ln.split("|", 1)
            self.pkgs.append(d)
            files |= {os.path.relpath(os.path.join(d, n), self.root) for n in names.split(",") if n}
        self.files = files
        tool = out / "goast"
        subprocess.run(["go", "build", "-o", str(tool), "."], cwd=Path(__file__).resolve().parent / "goast", check=True)
        r = subprocess.run([str(tool), *sorted(files)], cwd=self.root, capture_output=True, text=True, check=True)
        for ln in r.stdout.splitlines():
            j = json.loads(ln)
            q = f"{j['recv']}.{j['name']}" if j["recv"] else j["name"]
            self._defs.setdefault(j["file"], []).append(scan.Def(q, j["name"], j["line"], j["end"], j["exported"]))

    def sources(self) -> list:
        return sorted(self.files)

    def scan_defs(self, rel: str) -> list:
        return []

    def _one(self, cmd: list) -> str:
        r = subprocess.run(cmd, cwd=self.root, capture_output=True, text=True)
        return r.stdout + r.stderr

    def compile(self) -> list:
        """Every package, then each package with its tests: `go test ./...` stops a package's test
        build once the plain build fails, so test files need their own `go test -c`."""
        cmds = [["go", "build", "-gcflags=-e", "./..."]]
        cmds += [["go", "test", "-c", "-o", os.devnull, "-gcflags=-e", p] for p in self.pkgs]
        seen, out = set(), []
        for text in pool_map(self._one, cmds, self.jobs):
            for d in diag.parse_go(text):
                d.file = self.rel(str(self.root), d.file)
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

    def inactive(self):
        return set(self.files), {}

    def local_only(self, d) -> bool:
        return not d.public


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


# ---------- targets ----------

def all_defs(be) -> dict:
    """file -> definitions, over the source files the build compiles."""
    return {f: be.defs_in(f) for f in be.sources() if not (be.lang == "c" and f.endswith(".h"))}


def candidates(be, defs: dict, lo: int, hi: int, seed: int) -> tuple[list, list]:
    """Shuffled unique definitions with lo..hi call-like mentions, split by whether the name is
    also mentioned as a value. All counts come from the cleaned source, not from fox."""
    by = defaultdict(list)
    for f, ds in defs.items():
        for d in ds:
            if be.local_only(d):
                by[d.name].append((f, d))
    every = Counter(d.name for ds in defs.values() for d in ds)
    calls, values, macros = Counter(), Counter(), set()
    texts = list(defs) + ([str(p.relative_to(be.root)) for p in be.root.rglob("*.h")] if be.lang == "c" else [])
    for f in texts:
        t = be.text(f)
        macros.update(re.findall(r"#\s*define\s+([A-Za-z_]\w*)", t))
        cl = scan.clean(t, be.lang)
        for m in re.finditer(r"(\.|->|::)?\b([A-Za-z_]\w*)\b(\s*\()?", cl):
            if m[3]:
                calls[m[2]] += 1
            elif not m[1]:
                values[m[2]] += 1
    pool = sorted(q for q, xs in by.items() if len(xs) == 1 and every[q] == 1 and xs[0][1].bare not in macros
                  and lo + 1 <= calls[xs[0][1].bare] <= 2 * hi + 1 and len(xs[0][1].bare) > 3
                  and not xs[0][1].bare.startswith(("main", "test", "Test", "Benchmark")))
    rng = random.Random(seed)
    rng.shuffle(pool)
    out = [{"name": q, "path": by[q][0][0], "line": by[q][0][1].line, "bare": by[q][0][1].bare} for q in pool]
    return [t for t in out if values[t["bare"]] > 0], [t for t in out if values[t["bare"]] == 0]


# ---------- oracle run ----------

def run_one(be, t: dict, base: set, log) -> dict | None:
    t0 = time.time()
    bare = t["bare"]
    try:
        with Mutation(be.root) as m:
            ds = be.mutate(m, bare, t["path"], t["line"], base)
    except Skip as e:
        log(f"skip {t['name']}: {e}")
        return None
    sites, dropped = {}, []
    for d in ds:
        if d.sev not in ("error", "warning", "fatal error"):
            continue
        kind = be.classify(d, bare)
        if kind == "decl":
            continue
        f, ln = d.site(be.in_macro_def)
        if (f, ln) == (t["path"], t["line"]):
            continue
        prev = sites.get((f, ln))
        if prev and prev["kind"] != "other":
            continue
        name = be.enclosing(f, ln, d)
        via_macro = (f, ln) != (d.file, d.line)
        if name is None and kind != "other":
            name = None if via_macro else be.top_name(f, ln)
            if name is None:
                dropped.append(f"{f}:{ln} {kind} (file scope{' macro use' if via_macro else ''}, no item name)")
                continue
        sites[(f, ln)] = {"path": f, "line": ln, "kind": kind, "msg": d.msg[:200], "func": name or "(top)",
                          "code": be.text(f).split("\n")[ln - 1].strip()[:140]}
    for x in dropped:
        log(f"  {t['name']}: dropped {x}")
    r = dict(t, sites=sorted(sites.values(), key=lambda s: (s["path"], s["line"])), dropped=dropped)
    c = Counter(s["kind"] for s in r["sites"])
    log(f"{t['name']} {t['path']}:{t['line']}: {dict(c)} in {time.time() - t0:.0f}s")
    return r


def run_oracle(be, picks, n: int, lo: int, hi: int, base: set, log) -> list:
    """Oracle on candidates until n targets with lo..hi calling functions (or any store); half of
    them with stores when the candidates allow."""
    with_val, without = picks
    want_store, results, tried = n - n // 2, [], 0
    for group, need_store in ((with_val, True), (without + with_val, False)):
        for t in group:
            if len(results) >= n or tried >= 6 * n or need_store and sum(1 for r in results if r["stores"]) >= want_store:
                break
            if any(r["name"] == t["name"] for r in results):
                continue
            tried += 1
            r = run_one(be, t, base, log)
            if r is None:
                continue
            callers = {(s["path"], s["func"]) for s in r["sites"] if s["kind"] == "call"}
            r["stores"] = sum(1 for s in r["sites"] if s["kind"] == "store")
            if not r["sites"]:
                log(f"skip {t['name']}: the change broke nothing in this config")
            elif (lo <= len(callers) <= hi or r["stores"]) and len(callers) <= hi:
                results.append(r)
            else:
                log(f"skip {t['name']}: {len(callers)} calling functions")
    return results


# ---------- depth 2: callers of the callers ----------

def hop1_defs(be, r: dict, max_callers: int) -> list:
    """The definition of each function that calls the target, as run_one targets. Skip when one
    has no single definition in its file or (Rust) a trait fixes its signature, so a changed
    parameter list would not break its callers."""
    callers = sorted({(s["path"], s["func"]) for s in r["sites"] if s["kind"] == "call"})
    if len(callers) > max_callers:
        raise Skip(f"{len(callers)} callers, over {max_callers}")
    out = []
    for path, func in callers:
        ds = [d for d in be.defs_in(path) if d.name == func]
        if len(ds) != 1:
            raise Skip(f"caller {path} {func}: {len(ds)} definitions")
        if be.lang == "rust" and ds[0].trait:
            raise Skip(f"caller {path} {func}: trait method")
        out.append({"name": func, "path": path, "line": ds[0].line, "bare": ds[0].bare})
    return out


def impact_gold(r: dict, callers: dict) -> list:
    """Functions within two hops of direct calls of the target, each at its nearest hop.
    `callers` maps each hop-1 (path, func) to its own oracle sites."""
    hop = {(s["path"], s["func"]): 1 for s in r["sites"] if s["kind"] == "call"}
    for sites in callers.values():
        for s in sites:
            if s["kind"] == "call":
                hop.setdefault((s["path"], s["func"]), 2)
    hop.pop((r["path"], r["name"]), None)
    return [{"func": f, "path": p, "hop": h} for (p, f), h in sorted(hop.items(), key=lambda kv: (kv[1], kv[0]))]


def depth2_skip(hops: list, max_gold: int) -> str | None:
    if not any(h["hop"] == 2 for h in hops):
        return "no function two hops out"
    if len(hops) > max_gold:
        return f"{len(hops)} functions, over {max_gold}"
    first = {}
    for h in hops:
        if h["func"] in first:
            return f"{first[h['func']]} and {h['path']} both define {h['func']}; grading by name cannot tell them apart"
        first[h["func"]] = h["path"]
    return None


def run_depth2(be, results: list, base: set, log, max_callers: int, max_gold: int) -> list:
    """Two-hop impact ground truth: the oracle on every caller of each depth-1 target."""
    cache, out = {}, []
    for r in results:
        try:
            defs = hop1_defs(be, r, max_callers)
            callers = {}
            for t in defs:
                k = (t["path"], t["name"])
                if k not in cache:
                    cache[k] = run_one(be, t, base, log)
                if cache[k] is None:
                    raise Skip(f"caller {t['path']} {t['name']} could not be changed")
                callers[k] = cache[k]["sites"]
        except Skip as e:
            log(f"skip depth 2 {r['name']}: {e}")
            continue
        hops = impact_gold(r, callers)
        why = depth2_skip(hops, max_gold)
        if why:
            log(f"skip depth 2 {r['name']}: {why}")
        else:
            out.append({k: r[k] for k in ("name", "path", "line", "bare")} | {"hops": hops})
            log(f"depth 2 {r['name']}: {sum(h['hop'] == 1 for h in hops)} + {sum(h['hop'] == 2 for h in hops)} functions")
    return out


# ---------- fox vs oracle ----------

def fox_sites(fox, be, r) -> dict:
    callers = fox.graph(r["name"], "callers")
    refs = fox.graph(r["name"], "refs")
    call_at = {(s.path, s.line) for s in callers.sites}

    def nm(s):
        if s.who != "(top)":
            return s.who
        inside = scan.innermost(be.defs_in(s.path), s.line)
        return "(top)" if inside else (be.top_name(s.path, s.line) or "(top)")

    calls = {(s.path, s.line): (nm(s), s.guess) for s in callers.sites}
    vals = {(s.path, s.line): (nm(s), s.guess) for s in refs.sites if (s.path, s.line) not in call_at
            and (s.path, s.line) != (r["path"], r["line"])}
    return {"calls": calls, "values": vals, "resolved": callers.target}


def pr(tp, fp, fn) -> str:
    p = f"{tp / (tp + fp):.2f}" if tp + fp else "-"
    rc = f"{tp / (tp + fn):.2f}" if tp + fn else "-"
    return f"precision {p} ({tp}/{tp + fp}), recall {rc} ({tp}/{tp + fn})"


def def_misses(be, fox, defs: dict) -> tuple[int, int, list]:
    """Definitions the source scan finds that fox's outline of the same file lacks."""
    total, missed = 0, []
    for f, ds in defs.items():
        if not ds:
            continue
        have = Counter(s.name for s in fox.outline(f) if s.kind in foxcli.FN_KINDS)
        need = Counter(d.bare for d in ds)
        total += sum(need.values())
        for nm, k in need.items():
            if have[nm] < k:
                missed += [f"{f}:{d.line} {d.name}" for d in ds if d.bare == nm][: k - have[nm]]
    return total, len(missed), missed


def compare(results, fox, be, files: set, off: dict, config: str, defstat) -> str:
    total, nmiss, missed = defstat
    out = [f"fox callers vs compiler oracle. Config: {config}.",
           "Only code compiled in that config counts. Fox sites outside it are counted apart: `excluded (file)` for",
           "files the build does not compile, `excluded (config)` for inactive #if branches (C, from gcc -E) or cfg'd-off",
           "Rust items (heuristic: Linux x86_64, tests on, every feature on). They are neither hits nor false positives.",
           "call = fox `--rel callers` sites vs oracle too-few-argument sites.",
           "store = fox non-call refs (`--rel refs` minus callers) vs oracle function-pointer sites; fox non-call",
           "refs at sites the oracle saw as `other` (comparisons, casts) count as neither.",
           f"Definitions: fox's outline lacks {nmiss} of {total} function definitions the source scan finds"
           f" ({nmiss / total:.1%})." if total else "Definitions: none scanned.", ""]
    tot = Counter()
    rows = []
    for r in results:
        fs = fox_sites(fox, be, r)
        o_call = {(s["path"], s["line"]): s for s in r["sites"] if s["kind"] == "call"}
        o_store = {(s["path"], s["line"]): s for s in r["sites"] if s["kind"] == "store"}
        o_other = {(s["path"], s["line"]) for s in r["sites"] if s["kind"] == "other"}
        res = {}
        for kind, o, f in (("call", o_call, fs["calls"]), ("store", o_store, {k: v for k, v in fs["values"].items() if k not in o_other})):
            ex_file = {k for k in f if k not in o and k[0] not in files}
            ex_cfg = {k for k in f if k not in o and k not in ex_file and k[1] in off.get(k[0], ())}
            fk = set(f) - ex_file - ex_cfg
            tp, fp, fn = len(fk & o.keys()), len(fk - o.keys()), len(o.keys() - fk)
            of = {(k[0], v["func"]) for k, v in o.items()}
            ff = {(k[0], f[k][0]) for k in fk}
            c = {"tp": tp, "fp": fp, "fn": fn, "ftp": len(of & ff), "ffp": len(ff - of), "ffn": len(of - ff),
                 "exf": len(ex_file), "exc": len(ex_cfg), "gfp": sum(1 for k in fk - o.keys() if f[k][1])}
            tot.update({f"{kind}_{k}": v for k, v in c.items()})
            res[kind] = (c, f, o, fk, ex_file, ex_cfg)
        rows.append((r, fs, res))
    out.append(f"TOTAL over {len(results)} targets ({sum(1 for r in results if r['stores'])} with stores)")
    for kind in ("call", "store"):
        g = lambda k: tot[f"{kind}_{k}"]  # noqa: E731
        out.append(f"  {kind} sites:     {pr(g('tp'), g('fp'), g('fn'))}; fox sites excluded by config:"
                   f" {g('exf')} (file not built), {g('exc')} (#if/cfg); false positives fox marks guess: {g('gfp')}")
        out.append(f"  {kind} functions: {pr(g('ftp'), g('ffp'), g('ffn'))}")
    dropped = sum(len(r["dropped"]) for r in results)
    if dropped:
        out.append(f"  file-scope macro uses with no item name, left out of both sides: {dropped}")
    out.append("")
    for r, fs, res in rows:
        out.append(f"== {r['name']}  {r['path']}:{r['line']}  (fox resolved {fs['resolved'] or 'nothing'})")
        for kind in ("call", "store"):
            c, f, o, fk, ex_file, ex_cfg = res[kind]
            if not (c["tp"] or c["fp"] or c["fn"] or ex_file or ex_cfg):
                out.append(f"  {kind}: none either side")
                continue
            out.append(f"  {kind} sites: {pr(c['tp'], c['fp'], c['fn'])}; functions: {pr(c['ftp'], c['ffp'], c['ffn'])}")
            code = lambda k: be.text(k[0]).split("\n")[k[1] - 1].strip()[:120]  # noqa: E731
            for k in sorted(fk - o.keys()):
                g = " (fox marks guess)" if f[k][1] else ""
                out.append(f"    fox only  {k[0]}:{k[1]} {f[k][0]}{g} | {code(k)}")
            for k in sorted(o.keys() - fk):
                out.append(f"    oracle only {k[0]}:{k[1]} {o[k]['func']} | {o[k]['code']}")
            for k in sorted(ex_file):
                out.append(f"    excluded (file not built) {k[0]}:{k[1]} {f[k][0]}")
            for k in sorted(ex_cfg):
                out.append(f"    excluded (#if/cfg) {k[0]}:{k[1]} {f[k][0]}")
            for k in sorted(o.keys() & fk):
                if o[k]["func"] != f[k][0]:
                    out.append(f"    same site, fox names {f[k][0]}, source names {o[k]['func']}: {k[0]}:{k[1]}")
        for s in r["sites"]:
            if s["kind"] == "other":
                out.append(f"  other {s['path']}:{s['line']} {s['func']} | {s['msg'][:100]}")
        for x in r["dropped"]:
            out.append(f"  dropped {x}")
    out += ["", f"Definitions fox's outline lacks ({nmiss}):"] + [f"  {x}" for x in missed]
    return "\n".join(out) + "\n"


# ---------- fox's own labels ----------

def label_items(gold: set, said: list, via: set = frozenset()) -> tuple[dict, list]:
    """Fox's label for each gold (path, func): `certain` when fox lists it with at least one
    non-guess site, `guess` when only with guesses, else `missing`. `said` holds fox's
    (path, func, guess) rows; the functions fox lists that are not gold come back as extras,
    flagged `via` when fox reached them only through a function value."""
    best = {}
    for path, func, guess in said:
        if func != "(top)":
            best[(path, func)] = best.get((path, func), True) and guess
    labels = {k: ("guess" if best[k] else "certain") if k in best else "missing" for k in gold}
    extra = [{"path": p, "func": f, "fox": "guess" if g else "certain", "via": (p, f) in via}
             for (p, f), g in sorted(best.items()) if (p, f) not in gold]
    return labels, extra


def _built(path: str, line: int, files: set, off: dict) -> bool:
    return path in files and line not in off.get(path, ())


def fox_labels(fox, be, results: list, impacts: list, files: set, off: dict, log=print) -> dict:
    """task id -> (gold labels, extras) from fox's callers, refs and two-hop impact answers.
    Fox rows in code the build leaves out are neither gold nor wrong, so they are dropped."""
    out = {}
    for r in results:
        fs = fox_sites(fox, be, r)
        other = {(s["path"], s["line"]) for s in r["sites"] if s["kind"] == "other"}
        for kind, task, rows in (("call", "callers", fs["calls"]), ("store", "stores", fs["values"])):
            gold = {(s["path"], s["func"]) for s in r["sites"] if s["kind"] == kind}
            if not gold:
                continue
            said = [(p, who, g) for (p, ln), (who, g) in rows.items()
                    if _built(p, ln, files, off) and (kind == "call" or (p, ln) not in other)]
            out[f"oracle-{task}-{r['name']}"] = label_items(gold, said)
    for t in impacts:
        imp = fox.impact(t["name"], 2)
        if imp.target != f"{t['path']}:{t['line']}":
            log(f"WARNING: fox impact of {t['name']} resolved {imp.target or 'nothing'}, not {t['path']}:{t['line']}")
        rows = [x for x in imp.rows if x.depth <= 2 and _built(x.path, x.line, files, off)]
        out[f"oracle-impact2-{t['name']}"] = label_items(
            {(h["path"], h["func"]) for h in t["hops"]}, [(x.path, x.who, x.guess) for x in rows],
            {(x.path, x.who) for x in rows if x.via})
    return out


# ---------- tasks ----------

def q(s: str) -> str:
    return json.dumps(s, ensure_ascii=False)


def _items(gold: list, kind: str, labels: dict | None) -> str:
    """gold: (func, path, hop) triples."""
    def one(n, p, h):
        fox = f", fox = {q(labels.get((p, n), 'missing'))}" if labels is not None else ""
        return f"{{name = {q(n)}, file = {q(p)}, kind = {q(kind)}, hop = {h}{fox}}}"
    return "expect = [" + ", ".join(one(*g) for g in gold) + "]"


def _extra(extra: list) -> list:
    return ["fox_extra = [" + ", ".join(f"{{name = {q(e['func'])}, file = {q(e['path'])}, fox = {q(e['fox'])}, "
                                        f"via = {str(e['via']).lower()}}}" for e in extra) + "]"]


def tasks_toml(results, repo: str, config: str, impacts=(), labels: dict | None = None) -> str:
    """Depth-1 callers and stores tasks, then depth-2 impact tasks. With `labels` (fox_labels),
    each expected item carries fox's label and each task lists fox's other answers in fox_extra."""
    where = f"Only code compiled in this build counts ({config}); ignore code the build does not compile."
    out = [f"# Compiler-oracle tasks generated by bench/oracle/oracle.py. {where}",
           "# depth: hops of direct calls from the target; hop: each item's nearest hop; fox: fox's own label",
           "# for the item (certain, guess, missing); fox_extra: what fox answers that the oracle says is wrong.",
           f"repo = {q(repo)}", ""]

    def task(tid, depth, prompt, gold, kind):
        lab = labels.get(tid) if labels is not None else None
        return (["[[task]]", f"id = {q(tid)}", f"depth = {depth}", "prompt = " + q(prompt),
                 _items(gold, kind, lab[0] if lab else None)] + (_extra(lab[1]) if lab else []) + [""])

    for r in results:
        calls = sorted({(s["func"], s["path"], 1) for s in r["sites"] if s["kind"] == "call"})
        stores = sorted({(s["func"], s["path"], 1) for s in r["sites"] if s["kind"] == "store"})
        at = f"`{r['name']}` (defined at {r['path']}:{r['line']})"
        if calls:
            out += task("oracle-callers-" + r["name"], 1,
                        f"List every function in this repo that directly calls {at}. Count call expressions only, "
                        "including a call written through a macro (name the function where the macro is used). "
                        "Do not count places that store it in a function pointer, pass it as a callback, or put it "
                        f"in a table. {where} Answer with one line per caller: file path and function name. "
                        "Do not edit files.", calls, "call")
        if stores:
            out += task("oracle-stores-" + r["name"], 1,
                        f"List every place in this repo that uses {at} as a value without calling it: stores it "
                        "in a variable or struct field, puts it in a table initializer, passes it as a callback "
                        "argument, or returns it. Direct calls do not count. Name the enclosing function, or the "
                        f"variable for a file-scope initializer. {where} Answer with one line per place: file path "
                        "and function or variable name. Do not edit files.", stores, "store")
    for t in impacts:
        at = f"`{t['name']}` (defined at {t['path']}:{t['line']})"
        out += task("oracle-impact2-" + t["name"], 2,
                    f"List every function in this repo within two hops of direct calls of {at}: every function that "
                    "directly calls it, and every function that directly calls one of those callers. Count call "
                    "expressions only, including a call written through a macro (name the function where the macro "
                    "is used). Do not follow function pointers, callbacks, tables or interface dispatch. "
                    f"{where} Answer with one line per function: file path and function name. Do not edit files.",
                    [(h["func"], h["path"], h["hop"]) for h in t["hops"]], "call")
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
    ap.add_argument("--targets", help="comma-separated names (as the source scan names them) instead of picks")
    ap.add_argument("-n", type=int, default=20)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--min-callers", type=int, default=3)
    ap.add_argument("--max-callers", type=int, default=40)
    ap.add_argument("--depth", type=int, choices=[1, 2], default=2, help="2 adds two-hop impact tasks")
    ap.add_argument("--depth2-max-callers", type=int, default=10, help="depth 2: most direct callers to follow")
    ap.add_argument("--depth2-max-gold", type=int, default=40, help="depth 2: most functions in one task")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--no-index", action="store_true", help="skip `fox index` (store already current)")
    a = ap.parse_args()
    for s in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(s, restore_on_signal)
    root = a.repo.resolve()
    a.out.mkdir(parents=True, exist_ok=True)
    logf = (a.out / "log.txt").open("a")

    def log(s):
        print(s, flush=True)
        logf.write(s + "\n")
        logf.flush()

    be = {"c": lambda: C(root, a.compdb, a.jobs), "rust": lambda: Rust(root, a.jobs),
          "go": lambda: Go(root, a.jobs, a.out.resolve())}[a.lang]()
    left = leftovers(root, be.exts)
    if left:
        raise SystemExit(f"{MARK} already in {left[:5]}: restore these files first")
    config = a.config or {"c": "compile commands in --compdb", "rust": "cargo check --workspace --all-targets",
                          "go": "go build and go test -c of every package"}[a.lang]
    fox = foxcli.Fox(a.fox, a.fox_home, str(root))
    if not a.no_index:
        log(fox.index(str(root)).strip())
    t0 = time.time()
    base_diags = be.compile()  # also refreshes Rust dep-info
    base = {d.key() for d in base_diags}
    log(f"baseline: {len(base_diags)} diagnostics in {time.time() - t0:.0f}s")
    files, off = be.inactive()
    log(f"build: {len(files)} files, {sum(len(v) for v in off.values())} lines left out by #if/cfg")
    defs = all_defs(be)
    if a.targets:
        index = {d.name: (f, d) for f, ds in defs.items() for d in ds}
        picks = ([{"name": nm, "path": index[nm][0], "line": index[nm][1].line, "bare": index[nm][1].bare}
                  for nm in a.targets.split(",")], [])
    else:
        picks = candidates(be, defs, a.min_callers, a.max_callers, a.seed)
        log(f"candidates: {len(picks[0])} named as values too, {len(picks[1])} only called")
    results = run_oracle(be, picks, a.n, a.min_callers, a.max_callers, base, log)
    impacts = run_depth2(be, results, base, log, a.depth2_max_callers, a.depth2_max_gold) if a.depth == 2 else []
    left = leftovers(root, be.exts)
    if left:
        log(f"WARNING: {MARK} left in {left}")
    meta = {"lang": a.lang, "config": config, "commit": head_commit(root), "fox": fox.run("--version").strip()}
    (a.out / "oracle.json").write_text(json.dumps({"meta": meta, "targets": results, "impacts": impacts}, indent=1))
    labels = fox_labels(fox, be, results, impacts, files, off, log)
    (a.out / "tasks.toml").write_text(tasks_toml(results, a.task_repo or str(a.repo), config, impacts, labels))
    defstat = def_misses(be, fox, defs)
    text = compare(results, fox, be, files, off, f"{config}; commit {meta['commit'][:12]}; {meta['fox']}", defstat)
    (a.out / "comparison.txt").write_text(text)
    print(text)


if __name__ == "__main__":
    main()
