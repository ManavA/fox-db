"""`fox` CLI calls on a scratch FOXDB_HOME, and parsers for their text answers."""

import os
import re
import subprocess
from dataclasses import dataclass

HEAD = re.compile(r"^(callers|refs) of (\S+) (\S+) (.+?)\s+(\S+):(\d+)\s*$")
ROW = re.compile(r"^  :(?P<lines>\d+(?:,\d+)*) (?P<who>.+?)(?: \| .*)?$")
WHO = re.compile(r"^(?P<name>\(top\)|\S+?)(?: c:[0-9a-f]+)?(?P<guess> guess.*)?$")
OUTLINE = re.compile(r"^(?P<ind> *)(?P<line>\d+) (?P<kind>\S+) (?P<name>.+?) c:[0-9a-f]+(?: \.\.(?P<end>\d+))?\s*$")


@dataclass
class Site:
    path: str
    line: int
    who: str  # enclosing function as fox names it, "(top)" at file scope
    guess: bool


@dataclass
class Graph:
    target: str  # "path:line" of the definition fox resolved
    sites: list
    raw: str


class Fox:
    def __init__(self, bin_: str, home: str, cwd: str):
        self.bin, self.cwd = bin_, cwd
        self.env = dict(os.environ, FOXDB_HOME=home, FOXDB_NO_DAEMON="1")
        if os.path.realpath(home) == os.path.realpath(os.path.expanduser("~/.foxdb")):
            raise SystemExit("refusing to use the live ~/.foxdb; pass a scratch --fox-home")

    def run(self, *args: str) -> str:
        r = subprocess.run([self.bin, *args], cwd=self.cwd, env=self.env, capture_output=True, text=True, timeout=600)
        return r.stdout + r.stderr

    def index(self, path: str) -> str:
        return self.run("index", path, "--quiet", "--no-global")

    def graph(self, sym: str, rel: str) -> Graph:
        out = self.run("code", sym, "--rel", rel, "--budget", "400000")
        return parse_graph(out)

    def outline(self, path: str) -> list:
        return parse_outline(self.run("code", path, "--rel", "outline", "--budget", "400000"))


def parse_graph(text: str) -> Graph:
    """Rows of `fox code X --rel callers|refs`: one Site per listed line."""
    target, sites = "", []
    cur_dir, cur_file, on = "", None, False
    for raw in text.splitlines():
        m = HEAD.match(raw)
        if m:
            target, on = f"{m[5]}:{m[6]}", True
            continue
        if not on:
            continue
        if raw.startswith("~ ") or (raw and not raw.startswith(" ") and raw.endswith(":")):
            break  # provenance line, or a tail section (references, not calls / declarations)
        if raw.startswith("   "):
            continue  # code line under a multi-line row
        m = ROW.match(raw)
        if m and cur_file:
            w = WHO.match(m["who"].strip())
            who = w["name"] if w else m["who"]
            for ln in m["lines"].split(","):
                sites.append(Site(cur_file, int(ln), who, bool(w and w["guess"])))
            continue
        fm = re.match(r"^( ?)(\S.*) \((\d+)\)$", raw)
        if fm:
            cur_file = (cur_dir + fm[2]) if fm[1] else fm[2]
            if not fm[1]:
                cur_dir = ""
            continue
        if raw.endswith("/") and not raw.startswith(" "):
            cur_dir = raw
    return Graph(target, sites, text)


@dataclass
class Sym:
    line: int
    end: int | None
    kind: str
    name: str
    depth: int


def parse_outline(text: str) -> list:
    out = []
    for raw in text.splitlines()[1:]:
        m = OUTLINE.match(raw)
        if m:
            out.append(Sym(int(m["line"]), int(m["end"]) if m["end"] else None, m["kind"], m["name"], len(m["ind"])))
    return out


FN_KINDS = {"function", "method", "fn"}


def enclosing(outline: list, line: int) -> Sym | None:
    """Innermost function or method whose range holds `line`; a row without `..end` is one line."""
    best = None
    for s in outline:
        end = s.line if s.end is None else s.end
        if s.line <= line <= end and s.kind in FN_KINDS and (best is None or s.line >= best.line):
            best = s
    return best


