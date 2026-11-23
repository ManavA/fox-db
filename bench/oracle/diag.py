"""Parse compiler diagnostics and classify each error site against a mutated target.

Kinds: `call` (a call now has too few arguments), `store` (the function is used as a value:
stored, initialized, returned or passed as a function pointer), `other` (anything else new),
`decl` (a declaration that conflicts with the mutated one; the mutator fixes these).
"""

import json
import re
from dataclasses import dataclass, field

GCC_LINE = re.compile(r"^(?P<file>[^:\s][^:]*):(?P<line>\d+):(?P<col>\d+): (?P<sev>fatal error|error|warning|note): (?P<msg>.*)$")
GCC_CTX = re.compile(r"^(?P<file>[^:\s][^:]*): (?:In function [‘'](?P<fn>[^’']+)[’']|At top level):$")
Q = "[‘’'`\"]"


@dataclass
class Diag:
    file: str
    line: int
    col: int
    sev: str
    msg: str
    func: str | None = None  # enclosing function the compiler named, None at top level or unknown
    notes: list = field(default_factory=list)  # (file, line, col, msg)
    code: str = ""  # rustc error code
    span_text: str = ""  # rustc: the source the primary span highlights
    ctx: tuple | None = None  # gcc: the last "file: In function 'f':" line before it

    def site(self) -> tuple[str, int]:
        """Where the code that breaks is written: the outermost macro expansion, else the primary."""
        exp = [n for n in self.notes if n[3].startswith("in expansion of macro")]
        if exp:
            return exp[-1][0], exp[-1][1]
        return self.file, self.line

    def key(self) -> tuple:
        return (self.file, self.line, self.col, self.sev, self.msg)


def parse_gcc(text: str) -> list[Diag]:
    """gcc/clang text output (`-fdiagnostics-plain-output`): primaries with their notes."""
    out: list[Diag] = []
    ctx: tuple[str, str | None] | None = None
    for raw in text.splitlines():
        m = GCC_CTX.match(raw)
        if m:
            ctx = (m["file"], m["fn"])
            continue
        m = GCC_LINE.match(raw)
        if not m:
            continue
        f, ln, col, sev, msg = m["file"], int(m["line"]), int(m["col"]), m["sev"], m["msg"]
        if sev == "note":
            if out:
                out[-1].notes.append((f, ln, col, msg))
            continue
        out.append(Diag(f, ln, col, sev, msg, ctx=ctx))
    for d in out:
        sf, _ = d.site()
        d.func = d.ctx[1] if d.ctx and d.ctx[0] == sf else None
    return out


def classify_c(d: Diag, name: str) -> str:
    msg = d.msg
    if re.search(rf"conflicting types for {Q}{re.escape(name)}{Q}", msg):
        return "decl"
    if re.search(rf"too few arguments to function(?: call)?(?: {Q}{re.escape(name)}{Q})?", msg):
        return "call"
    if "incompatible pointer type" in msg or "incompatible function pointer type" in msg or "incompatible return type" in msg:
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
        s = spans[0]
        while s.get("expansion") and s["expansion"].get("span"):
            s = s["expansion"]["span"]  # macro use site
        code = (m.get("code") or {}).get("code") or ""
        sn = (spans[0].get("text") or [{}])[0]
        piece = sn.get("text", "")[sn.get("highlight_start", 1) - 1 : sn.get("highlight_end", 1) - 1]
        out.append(Diag(s["file_name"], s["line_start"], s["column_start"], m["level"], m["message"], code=code,
                        span_text=piece))
    return out


def classify_rust(d: Diag, name: str) -> str:
    if d.code == "E0061":
        return "call"
    if d.code in ("E0308", "E0593", "E0631", "E0277", "E0618"):
        if re.search(rf"(^|::|\b){re.escape(name)}\s*$", d.span_text.strip()):
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
    if d.msg.startswith("not enough arguments in call to"):
        return "call"
    if re.match(rf"cannot use {re.escape(name)}\b|cannot use [\w.]*\.{re.escape(name)}\b", d.msg) and "func(" in d.msg:
        return "store"
    return "other"
