"""Parse compiler diagnostics and classify each error site against a mutated target.

Kinds: `call` (a call to the target now has too few arguments), `store` (the target used as a
value: stored, initialized, returned or passed as a function pointer), `other` (anything else
new), `decl` (a declaration that conflicts with the mutated one; the mutator fixes these).
"""

import json
import re
from dataclasses import dataclass, field

GCC_LINE = re.compile(r"^(?:(?P<file>[^:\s][^:]*):)?(?P<line>\d+):(?P<col>\d+): (?P<sev>fatal error|error|warning|note): (?P<msg>.*)$")
GCC_CTX = re.compile(r"^(?P<file>[^:\s][^:]*): (?:In function [‘'](?P<fn>[^’']+)[’']|At top level):$")
GCC_FILE = re.compile(r"^(?P<file>[^\s:]+)$")  # gcc 16 groups rows of one file under its name
Q = "[‘’'`\"]"


@dataclass
class Diag:
    file: str  # spelling location: where the code the compiler complains about is written
    line: int
    col: int
    sev: str
    msg: str
    notes: list = field(default_factory=list)  # (file, line, col, msg)
    exp: list = field(default_factory=list)  # macro expansion sites, innermost first
    code: str = ""  # rustc error code
    span_text: str = ""  # rustc: the source the primary span highlights
    ctx: tuple | None = None  # gcc: (file, function) of the last "In function" line before it

    def site(self, in_macro_def=None) -> tuple[str, int]:
        """The spelling location, or the outermost expansion when the spelling is inside a macro
        definition (`in_macro_def(file, line)`); a call in a macro argument keeps its own line."""
        if self.exp and in_macro_def is not None and in_macro_def(self.file, self.line):
            return self.exp[-1]
        return self.file, self.line

    def func_at(self, file: str) -> str | None:
        """The function gcc said the diagnostic is in, when that context is in `file`."""
        return self.ctx[1] if self.ctx and self.ctx[0] == file else None

    def key(self) -> tuple:
        return (self.file, self.line, self.col, self.sev, self.msg)


def parse_gcc(text: str) -> list[Diag]:
    """gcc/clang text output (`-fdiagnostics-plain-output`): primaries with their notes."""
    out: list[Diag] = []
    ctx, cur = None, None
    for raw in text.splitlines():
        m = GCC_CTX.match(raw)
        if m:
            ctx = (m["file"], m["fn"])
            continue
        m = GCC_LINE.match(raw)
        if not m:
            fm = GCC_FILE.match(raw)
            if fm:
                cur = fm["file"]
            continue
        f = m["file"] or cur
        if f is None:
            continue
        cur = f
        ln, col, sev, msg = int(m["line"]), int(m["col"]), m["sev"], m["msg"]
        if sev == "note":
            if out:
                out[-1].notes.append((f, ln, col, msg))
                if msg.startswith("in expansion of macro"):
                    out[-1].exp.append((f, ln))
            continue
        out.append(Diag(f, ln, col, sev, msg, ctx=ctx))
    return out


def _named(text: str, name: str) -> bool:
    return re.search(rf"{Q}{re.escape(name)}{Q}", text) is not None


def classify_c(d: Diag, name: str) -> str:
    msg, n = d.msg, re.escape(name)
    if re.search(rf"conflicting types for {Q}{n}{Q}", msg):
        return "decl"
    if re.search(rf"too few arguments to function {Q}{n}{Q}", msg):
        return "call"
    names_target = any(_named(nm[3], name) and "declared here" in nm[3] for nm in d.notes) or _named(msg, name)
    if re.search(r"too few arguments to function call", msg) and names_target:
        return "call"  # clang does not name the callee in the message
    if names_target and re.search(r"incompatible (?:function )?pointer type|incompatible return type", msg):
        return "store"
    return "other"


def conflicting_decls(diags: list[Diag], name: str) -> list[tuple[str, int]]:
    """Locations of earlier declarations a mutated definition conflicts with."""
    out = []
    for d in diags:
        if classify_c(d, name) != "decl":
            continue
        for f, ln, _, msg in d.notes:
            if re.search(rf"previous (?:declaration|definition|implicit declaration) of {Q}{re.escape(name)}{Q}", msg):
                out.append((f, ln))
    return out


def parse_rustc_json(text: str) -> list[Diag]:
    """`cargo check --message-format=json` lines: compiler errors with their primary span."""
    out = []
    for raw in text.splitlines():
        try:
            j = json.loads(raw)
        except json.JSONDecodeError:
            continue
        m = j.get("message") if j.get("reason") == "compiler-message" else None
        if not m or m.get("level") not in ("error", "warning"):
            continue
        spans = [s for s in m.get("spans", []) if s.get("is_primary")]
        if not spans:
            continue
        s, exp = spans[0], []
        while s.get("expansion") and s["expansion"].get("span"):
            s = s["expansion"]["span"]
            exp.append((s["file_name"], s["line_start"]))
        p = spans[0]
        piece = "\n".join(t.get("text", "")[t.get("highlight_start", 1) - 1 : t.get("highlight_end", 1) - 1]
                          for t in p.get("text", []))
        code = (m.get("code") or {}).get("code") or ""
        out.append(Diag(p["file_name"], p["line_start"], p["column_start"], m["level"], m["message"], exp=exp,
                        code=code, span_text=piece))
    return out


def classify_rust(d: Diag, name: str) -> str:
    named = re.search(rf"\b{re.escape(name)}\b", d.span_text) is not None
    if d.code == "E0061" and named:
        return "call"
    if d.code in ("E0308", "E0593", "E0631", "E0277", "E0618") and named:
        return "store"
    return "other"


GO_LINE = re.compile(r"^(?:vet: )?(?P<file>[^:\s][^:]*\.go):(?P<line>\d+):(?P<col>\d+): (?P<msg>.*)$")
GO_SHORT = re.compile(r"^(?P<line>\d+):(?P<col>\d+): (?P<msg>.*)$")


def parse_go(text: str) -> list[Diag]:
    """`go build`/`go test` compile errors: `file.go:L:C: msg`, or a `file.go` line then `L:C: msg` rows."""
    out, cur = [], None
    for raw in text.splitlines():
        s = raw.strip()
        if s.endswith(".go") and " " not in s:
            cur = s
            continue
        m = GO_LINE.match(s)
        if m:
            out.append(Diag(m["file"], int(m["line"]), int(m["col"]), "error", m["msg"]))
            continue
        m = GO_SHORT.match(raw)
        if m and cur:
            out.append(Diag(cur, int(m["line"]), int(m["col"]), "error", m["msg"]))
    return out


def classify_go(d: Diag, name: str) -> str:
    m = re.match(r"not enough arguments in call to (\S+)", d.msg)
    if m:
        return "call" if m[1].split(".")[-1] == name else "other"
    if re.match(rf"cannot use (?:[\w.()*]*\.)?{re.escape(name)}\b", d.msg) and "func(" in d.msg:
        return "store"
    return "other"
